// RF5C164 — PCM дисковой приставки Sega Mega CD (и родич RF5C68).
//
// Восемь каналов, 8-битный PCM в представлении «знак-величина», общее ОЗУ
// сэмплов на 64 КБ. На рипах Mega CD на нём держится основная масса
// музыки: у Sonic CD в заголовке больше вообще ничего не объявлено.
//
// Модель снята с эталонной реализации (libvgm, emu/cores/rf5c68.c, оттуда
// же в MAME), а не выведена из описания.
//
// ОЗУ ЖИВЁТ В PSRAM, а не в блочной памяти: 64 КБ это 64 блока M10K, а
// свободно у нас четырнадцать. Модуль сам в память не ходит — он выдаёт
// адрес и ждёт байт, а обслуживает его арбитр chipbox, как уже устроено
// у OKIM6295 и K053260.
//
// Адрес канала 27-битный: старшие 16 бит — байт в ОЗУ, младшие 11 —
// дробная часть. Шаг задаётся регистрами FDL/FDH.

module rf5c164 (
    input wire clk,
    input wire cen,          // частота чипа / 384 (у Mega CD 12.5 МГц -> 32552 Гц)
    input wire rst,

    // Запись в регистры: 0x00-0x08 по описанию чипа
    input wire wr,
    input wire [3:0] addr,
    input wire [7:0] din,

    // Чтение ОЗУ сэмплов через арбитр: поднимаем rd с адресом и ждём
    // data_valid. Один запрос за раз, канал за каналом.
    output reg [15:0] mem_addr = 0,
    output reg mem_rd = 0,
    input wire [7:0] mem_data,
    input wire mem_valid,

    output reg signed [15:0] snd_l = 0,
    output reg signed [15:0] snd_r = 0
);

  // ------------------------------------------------------------------
  // Регистры

  reg [7:0] env[0:7];        // громкость канала
  reg [7:0] pan[0:7];        // младший полубайт — левый, старший — правый
  reg [15:0] step[0:7];      // шаг адреса, 5.11
  reg [15:0] loopst[0:7];    // начало петли, в байтах
  reg [7:0] start[0:7];      // стартовая страница (адрес = start << 19)
  reg [26:0] caddr[0:7];     // текущий адрес, 16.11
  reg [7:0] ch_on = 8'h00;   // бит канала: 1 — играет
  reg chip_en = 0;
  reg [2:0] cbank = 0;       // выбранный канал для записи регистров

  integer k;
  always @(posedge clk) begin
    if (rst) begin
      chip_en <= 0;
      ch_on <= 0;
      cbank <= 0;
      for (k = 0; k < 8; k = k + 1) begin
        env[k] <= 0; pan[k] <= 0; step[k] <= 0;
        loopst[k] <= 0; start[k] <= 0; caddr[k] <= 0;
      end
    end else if (wr) begin
      case (addr)
        4'h0: env[cbank] <= din;
        4'h1: pan[cbank] <= din;
        4'h2: step[cbank][7:0] <= din;
        4'h3: step[cbank][15:8] <= din;
        4'h4: loopst[cbank][7:0] <= din;
        4'h5: loopst[cbank][15:8] <= din;
        4'h6: begin
          start[cbank] <= din;
          // Пока канал молчит, запись стартовой страницы сразу двигает
          // текущий адрес — так в эталоне.
          if (!ch_on[cbank]) caddr[cbank] <= {din, 19'd0};
        end
        4'h7: begin
          chip_en <= din[7];
          if (din[6]) cbank <= din[2:0];
          // Бит 6 сброшен — это выбор страницы окна записи в ОЗУ (4 КБ).
          // Само окно живёт не здесь: адрес складывает парсер, который
          // ведёт ту же страницу и шлёт нам уже полные 16 бит. Так же
          // устроено и в эталоне (libvgm, DoRAMOfsPatches).
        end
        4'h8: begin
          // Бит канала СБРОШЕН — канал играет. Выключение возвращает
          // адрес на стартовую страницу.
          for (k = 0; k < 8; k = k + 1) begin
            ch_on[k] <= ~din[k];
            if (din[k]) caddr[k] <= {start[k], 19'd0};
          end
        end
        default: ;
      endcase
    end else if (step_ch) begin
      // Индекс канала защёлкивается ВМЕСТЕ с адресом: в том же такте
      // автомат уже переключил cur на следующий канал, и запись по cur
      // легла бы не туда.
      caddr[step_idx] <= next_addr;
    end
  end

  // ------------------------------------------------------------------
  // Автомат обхода каналов.
  //
  // За один такт cen надо обойти восемь каналов: у каждого прочитать байт
  // по своему адресу и добавить в сумму. Читаем по одному, дожидаясь
  // ответа памяти, — восьми чтений на 32.5 кГц арбитру хватает с запасом.

  localparam S_IDLE = 2'd0, S_REQ = 2'd1, S_WAIT = 2'd2, S_LOOP = 2'd3;
  reg [1:0] state = S_IDLE;
  reg [2:0] cur = 0;
  reg looped = 0;            // на этом канале уже прыгали в петлю
  reg signed [20:0] acc_l = 0, acc_r = 0;
  reg step_ch = 0;
  reg [2:0] step_idx = 0;
  reg [26:0] next_addr = 0;

  wire [11:0] mul_l = {4'd0, pan[cur][3:0]} * env[cur];
  wire [11:0] mul_r = {4'd0, pan[cur][7:4]} * env[cur];
  wire [6:0] mag = mem_data[6:0];

  // Произведение считается ШИРОКИМ и отдельной строкой.
  //
  // Раньше оно стояло прямо внутри конкатенации, а это самоопределённый
  // контекст: Verilog берёт ширину по большему операнду, то есть 12 бит,
  // и молча обрезает. 63 * 3825 = 240975 требует восемнадцати — выход
  // терял шесть разрядов и падал с ~1900 до 31.
  wire [18:0] prod_l = mag * mul_l;
  wire [18:0] prod_r = mag * mul_r;
  wire signed [20:0] amp_l = $signed({7'd0, prod_l[18:5]});
  wire signed [20:0] amp_r = $signed({7'd0, prod_r[18:5]});

  // Знак-величина: старший бит задаёт сторону, остальные семь — амплитуду
  wire signed [20:0] add_l = mem_data[7] ? amp_l : -amp_l;
  wire signed [20:0] add_r = mem_data[7] ? amp_r : -amp_r;

  always @(posedge clk) begin
    step_ch <= 0;
    mem_rd <= 0;
    if (rst) begin
      state <= S_IDLE; cur <= 0; acc_l <= 0; acc_r <= 0;
      snd_l <= 0; snd_r <= 0; looped <= 0;
    end else begin
      case (state)
        S_IDLE:
          if (cen) begin
            acc_l <= 0; acc_r <= 0; cur <= 0; looped <= 0;
            state <= S_REQ;
          end
        S_REQ: begin
          if (chip_en && ch_on[cur]) begin
            mem_addr <= caddr[cur][26:11];
            mem_rd <= 1;
            state <= S_WAIT;
          end else begin
            // канал молчит — сразу к следующему
            if (cur == 3'd7) begin
              snd_l <= sat(acc_l >>> 2); snd_r <= sat(acc_r >>> 2);
              state <= S_IDLE;
            end else begin
              cur <= cur + 1'b1; looped <= 0;
            end
          end
        end
        S_WAIT:
          if (mem_valid) begin
            if (mem_data == 8'hFF) begin
              // Метка конца: прыжок на начало петли. Если и там метка,
              // канал считается мёртвым и молчит до следующего запуска.
              if (looped) begin
                if (cur == 3'd7) begin
                  snd_l <= sat(acc_l >>> 2); snd_r <= sat(acc_r >>> 2);
                  state <= S_IDLE;
                end else begin
                  cur <= cur + 1'b1; looped <= 0; state <= S_REQ;
                end
              end else begin
                next_addr <= {loopst[cur], 11'd0};
                step_idx <= cur;
                step_ch <= 1;
                looped <= 1;
                state <= S_LOOP;
              end
            end else begin
              acc_l <= acc_l + add_l;
              acc_r <= acc_r + add_r;
              next_addr <= caddr[cur] + {11'd0, step[cur]};
              step_idx <= cur;
              step_ch <= 1;
              if (cur == 3'd7) begin
                snd_l <= sat((acc_l + add_l) >>> 2); snd_r <= sat((acc_r + add_r) >>> 2);
                state <= S_IDLE;
              end else begin
                cur <= cur + 1'b1; looped <= 0; state <= S_REQ;
              end
            end
          end
        S_LOOP: state <= S_REQ;   // адрес обновлён, читаем снова
        default: state <= S_IDLE;
      endcase
    end
  end

  // Выход делится на четыре: у восьми каналов на полной громкости сумма
  // доходит до 121 тысячи, а наружу идут 16 бит. Без запаса мы упирались
  // бы в потолок уже на трёх громких каналах. Потерю компенсирует гейн.
  function automatic signed [15:0] sat(input signed [20:0] v);
    sat = v > 32767 ? 16'sd32767 : v < -32768 ? -16'sd32768 : v[15:0];
  endfunction

endmodule
