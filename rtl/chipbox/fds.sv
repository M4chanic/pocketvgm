// Звуковой канал дисковой приставки Famicom (Famicom Disk System).
//
// Волновая таблица на 64 шага по 6 бит, два генератора огибающей
// (громкость и модулятор) и модулятор со своей таблицей на 64 шага по 3
// бита. На выходе — произведение отсчёта таблицы на громкость.
//
// Модель снята с эталонной реализации (libvgm, emu/cores/np_nes_fds.c,
// авторство NSFPlay), а не выведена из описания: там же взяты и странные
// правила округления в расчёте модуляции, которые из документации не
// следуют. Отображение адресов взято из того же libvgm
// (player/vgmplayer_cmdhandler.cpp, Cmd_NES_Reg).
//
// Тик — один такт разрешения `cen` (частота процессора Famicom,
// 1.789773 МГц). Эталон умеет тикать пачкой, нам это не нужно: за один
// такт ни фаза таблицы, ни фаза модулятора не могут перешагнуть больше
// одного шага, потому что частота не превышает 4095 из 65536.

module fds (
    input wire clk,
    input wire cen,
    input wire rst,

    // Запись: адрес в пространстве $4040-$408A (младшие 8 бит)
    input wire wr,
    input wire [7:0] addr,
    input wire [7:0] din,

    output wire signed [15:0] snd
);

  // ------------------------------------------------------------------
  // Состояние

  reg [5:0] wave_tbl[0:63];   // волновая таблица, 6 бит без знака
  reg [2:0] mod_tbl[0:63];    // таблица модулятора, 3 бита

  reg [11:0] freq_wav = 0, freq_mod = 0;
  reg [21:0] phase_wav = 0, phase_mod = 0;   // 6 бит индекса + 16 дробных

  reg env_dis_vol = 1, env_dis_mod = 1;
  reg env_mode_vol = 0, env_mode_mod = 0;
  reg [5:0] env_spd_vol = 0, env_spd_mod = 0;
  reg [5:0] env_out_vol = 0, env_out_mod = 0;
  reg [19:0] env_tmr_vol = 0, env_tmr_mod = 0;
  reg [7:0] master_env_spd = 0;

  reg wav_halt = 1, env_halt = 0, mod_halt = 1;
  reg wav_write = 0;
  reg [1:0] master_vol = 0;
  reg [6:0] mod_pos = 0;
  // $4023 бит 1: общее разрешение ввода-вывода. Пока не поднято, ни одна
  // запись в звуковые регистры не проходит — так же, как на приставке.
  reg io_en = 0;

  // ------------------------------------------------------------------
  // Запись в регистры

  integer i;
  always @(posedge clk) begin
    if (rst) begin
      freq_wav <= 0; freq_mod <= 0;
      phase_wav <= 0; phase_mod <= 0;
      env_dis_vol <= 1; env_dis_mod <= 1;
      env_mode_vol <= 0; env_mode_mod <= 0;
      env_spd_vol <= 0; env_spd_mod <= 0;
      env_out_vol <= 0; env_out_mod <= 0;
      env_tmr_vol <= 0; env_tmr_mod <= 0;
      master_env_spd <= 0;
      wav_halt <= 1; env_halt <= 0; mod_halt <= 1;
      wav_write <= 0; master_vol <= 0; mod_pos <= 0;
      io_en <= 0;
      for (i = 0; i < 64; i = i + 1) begin
        wave_tbl[i] <= 0;
        mod_tbl[i] <= 0;
      end
    end else if (wr && addr == 8'h23) begin
      io_en <= din[1];
    end else if (wr && io_en) begin
      if (addr >= 8'h40 && addr < 8'h80) begin
        // $4040-$407F: волновая таблица, и только при разрешённой записи
        if (wav_write) wave_tbl[addr[5:0]] <= din[5:0];
      end else if (addr >= 8'h80) begin
        case (addr)
          8'h80: begin  // огибающая громкости
            env_dis_vol <= din[7];
            env_mode_vol <= din[6];
            env_tmr_vol <= 0;
            env_spd_vol <= din[5:0];
            if (din[7]) env_out_vol <= din[5:0];
          end
          8'h82: freq_wav[7:0] <= din;
          8'h83: begin
            freq_wav[11:8] <= din[3:0];
            wav_halt <= din[7];
            env_halt <= din[6];
            if (din[7]) phase_wav <= 0;
            if (din[6]) begin
              env_tmr_vol <= 0;
              env_tmr_mod <= 0;
            end
          end
          8'h84: begin  // огибающая модулятора
            env_dis_mod <= din[7];
            env_mode_mod <= din[6];
            env_tmr_mod <= 0;
            env_spd_mod <= din[5:0];
            if (din[7]) env_out_mod <= din[5:0];
          end
          8'h85: mod_pos <= din[6:0];
          8'h86: freq_mod[7:0] <= din;
          8'h87: begin
            freq_mod[11:8] <= din[3:0];
            mod_halt <= din[7];
            // сброс дробной части фазы, индекс остаётся
            if (din[7]) phase_mod <= {phase_mod[21:16], 16'd0};
          end
          8'h88: begin
            // Запись в таблицу модулятора идёт по ТЕКУЩЕЙ позиции
            // воспроизведения — прямого способа задать фазу нет. Каждая
            // запись кладёт значение дважды и продвигает индекс на два.
            if (mod_halt) begin
              mod_tbl[phase_mod[21:16]] <= din[2:0];
              mod_tbl[phase_mod[21:16] + 6'd1] <= din[2:0];
              phase_mod <= {phase_mod[21:16] + 6'd2, phase_mod[15:0]};
            end
          end
          8'h89: begin
            wav_write <= din[7];
            master_vol <= din[1:0];
          end
          8'h8A: begin
            master_env_spd <= din;
            env_tmr_vol <= 0;
            env_tmr_mod <= 0;
          end
          default: ;
        endcase
      end
    end else if (cen) begin
      // ----------------------------------------------------------------
      // Тик

      // Огибающие. Период = (скорость + 1) * общая скорость * 8.
      if (!env_halt && !wav_halt && master_env_spd != 0) begin
        if (!env_dis_vol) begin
          if (env_tmr_vol + 20'd1 >= env_per_vol) begin
            env_tmr_vol <= 0;
            if (env_mode_vol) begin
              if (env_out_vol < 6'd32) env_out_vol <= env_out_vol + 1'b1;
            end else if (env_out_vol > 0) env_out_vol <= env_out_vol - 1'b1;
          end else env_tmr_vol <= env_tmr_vol + 20'd1;
        end
        if (!env_dis_mod) begin
          if (env_tmr_mod + 20'd1 >= env_per_mod) begin
            env_tmr_mod <= 0;
            if (env_mode_mod) begin
              if (env_out_mod < 6'd32) env_out_mod <= env_out_mod + 1'b1;
            end else if (env_out_mod > 0) env_out_mod <= env_out_mod - 1'b1;
          end else env_tmr_mod <= env_tmr_mod + 20'd1;
        end
      end

      // Модулятор: шаг таблицы двигает mod_pos по таблице смещений,
      // значение 4 сбрасывает позицию в ноль.
      if (!mod_halt) begin
        phase_mod <= phase_mod_next[21:0];
        if (mod_stepped) begin
          if (mod_tbl[phase_mod_next[21:16]] == 3'd4) mod_pos <= 0;
          else mod_pos <= mod_pos + mod_bias(mod_tbl[phase_mod_next[21:16]]);
        end
      end

      // Волновая таблица: частота смещается модулятором.
      // Шаг ЗНАКОВЫЙ: модулятор гнёт частоту в обе стороны, и при
      // отрицательном шаге фаза идёт назад. Расширение нулями сделало бы
      // из этого огромный положительный скачок.
      if (!wav_halt) phase_wav <= phase_wav + {{6{wav_step[15]}}, wav_step};

      // Выход обновляется, пока не идёт запись в таблицу.
      if (!wav_write) fout <= $signed({1'b0, wave_tbl[phase_wav[21:16]]}) * $signed({1'b0, vol_capped});
    end
  end

  wire [19:0] env_per_vol = ({14'd0, env_spd_vol} + 20'd1) * {12'd0, master_env_spd} * 20'd8;
  wire [19:0] env_per_mod = ({14'd0, env_spd_mod} + 20'd1) * {12'd0, master_env_spd} * 20'd8;

  wire [22:0] phase_mod_next = {1'b0, phase_mod} + {11'd0, freq_mod};
  wire mod_stepped = phase_mod_next[21:16] != phase_mod[21:16];

  function automatic signed [7:0] mod_bias(input [2:0] v);
    case (v)
      3'd0: mod_bias = 8'sd0;
      3'd1: mod_bias = 8'sd1;
      3'd2: mod_bias = 8'sd2;
      3'd3: mod_bias = 8'sd4;
      3'd4: mod_bias = 8'sd0;
      3'd5: mod_bias = -8'sd4;
      3'd6: mod_bias = -8'sd2;
      default: mod_bias = -8'sd1;
    endcase
  endfunction

  // ------------------------------------------------------------------
  // Расчёт модуляции. Правила округления здесь нарочно странные — они
  // сняты с эталона дословно, из документации они не следуют.

  wire signed [7:0] mod_signed = mod_pos < 7'd64 ? $signed({1'b0, mod_pos})
                                                 : $signed({1'b0, mod_pos}) - 8'sd128;
  wire signed [15:0] mod_mul = mod_signed * $signed({1'b0, env_out_mod});
  wire [3:0] mod_rem = mod_mul[3:0];
  wire signed [15:0] mod_sh = mod_mul >>> 4;
  // «+2 вверх, -1 вниз», если остаток не нулевой и бит 7 результата чист
  wire signed [15:0] mod_rnd = (mod_rem != 0 && mod_sh[7] == 1'b0)
                             ? (mod_signed < 0 ? mod_sh - 16'sd1 : mod_sh + 16'sd2)
                             : mod_sh;
  // завернуть в диапазон -64..191
  wire signed [15:0] mod_wrap = mod_rnd >= 16'sd192 ? mod_rnd - 16'sd256
                              : mod_rnd < -16'sd64 ? mod_rnd + 16'sd256 : mod_rnd;
  // Две ступени регистров: первое умножение с округлением и завёрткой,
  // потом второе. Одной цепочкой это давало путь 24 нс при периоде 17.5.
  // Входы (mod_pos, огибающая, freq_wav) меняются по тику cen раз в 32
  // такта, так что к следующему тику ступени давно сошлись; отличие от
  // прежнего только если регистр записан за такт до тика — тогда новое
  // значение войдёт в шаг тиком позже.
  reg signed [15:0] mod_wrap_r = 0;
  reg signed [23:0] mod_amount_r = 0;
  wire signed [23:0] mod_pitch = $signed({12'd0, freq_wav}) * mod_wrap_r;
  // сдвиг на 6 с округлением к ближайшему
  wire signed [23:0] mod_final = (mod_pitch >>> 6) + ((mod_pitch[5:0] >= 6'd32) ? 24'sd1 : 24'sd0);
  wire signed [23:0] mod_amount = env_out_mod != 0 ? mod_final : 24'sd0;
  always @(posedge clk) begin
    mod_wrap_r <= mod_wrap;
    mod_amount_r <= mod_amount;
  end
  // Ширина с запасом: freq_wav до 4095, модуляция до примерно +-12000.
  wire signed [15:0] wav_step = $signed({4'd0, freq_wav}) + mod_amount_r[15:0];

  wire [5:0] vol_capped = env_out_vol > 6'd32 ? 6'd32 : env_out_vol;

  reg signed [15:0] fout = 0;

  // Общая громкость: 0 = полная, дальше 2/3, 2/4, 2/5 по описанию чипа.
  // Деления заменены умножением на константу со сдвигом: на диапазоне
  // fout 0..2016 (63 * 32) результат совпадает с целочисленным делением
  // точно, проверено перебором. Делитель lpm_divide стоял в одной
  // цепочке с умножением громкости и не укладывался в такт; выход теперь
  // регистр, с задержкой в такт.
  reg signed [15:0] snd_q = 0;
  wire [27:0] vol23 = fout * 28'd2731;   // (f * 2) / 3 == (f * 2731) >> 12
  wire [27:0] vol25 = fout * 28'd3277;   // (f * 2) / 5 == (f * 3277) >> 13
  always @(posedge clk)
    snd_q <= master_vol == 2'd0 ? fout
           : master_vol == 2'd1 ? $signed({1'b0, vol23[26:12]})
           : master_vol == 2'd2 ? (fout >>> 1)
                                : $signed({1'b0, vol25[27:13]});
  assign snd = snd_q;

endmodule
