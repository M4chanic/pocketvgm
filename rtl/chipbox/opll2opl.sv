// Транслятор регистров OPLL (YM2413 / VRC7) в регистры OPL2.
//
// Своего OPLL в проекте нет, а OPL3 (opl3_fpga, урезанный до OPL2) уже
// стоит. OPLL — та же операторная схема Yamaha: два оператора на канал,
// те же огибающие, множители, KSL, обратная связь и полусинусоида как
// вторая волна. Отличия регистровые: инструменты зашиты в ПЗУ (15 штук
// плюс один пользовательский), громкость канала задаётся 4 битами вместо
// 6 бит TL несущей, F-число 9-битное вместо 10, и три правила отпускания
// клавиши, которых у OPL нет. Всё это переводится записями в OPL2, что
// модуль и делает: на каждую запись OPLL выдаёт от одной до тринадцати
// записей OPL2, которые секвенсор chipbox проталкивает в OPL3.
//
// Эталон — emu2413 (libvgm): таблицы патчей взяты оттуда байт в байт, а
// правила отпускания — из get_parameter_rate. У OPLL при key-off несущая
// уходит в release со скоростью 5 при взведённом бите sustain, со своей
// RR при sustained-огибающей (EG=1) и 7 при ударной; модулятор при снятой
// клавише замирает (скорость 0). У OPL2 обе ячейки уходят в release со
// своей RR — поэтому перед снятием клавиши модуль переписывает RR обеих
// ячеек нужными значениями, а при нажатии возвращает патчевые.
//
// Что не переводится и остаётся приближением: фаза DAMP у OPLL (быстрый
// сброс огибающей перед новым нажатием — OPL2 начинает атаку с текущего
// уровня), точная форма таблиц LFO и 9-битный ЦАП OPLL.
//
// KSL: коды OPLL 1 и 2 означают 1.5 и 3 дБ/окт, у OPL3 (и opl3_fpga,
// ksl_add_rom) наоборот — переставляются.
//
// Режим ритма YM2413 (регистр 0x0E) ложится на ритм-режим OPL2 (0xBD):
// биты в том же порядке, ячейки те же (канал 6 — бочка, 7 — хай-хэт и
// малый, 8 — том и тарелка); патчи ритма загружаются из тех же таблиц.
// У VRC7 ритма и каналов 6-8 нет — записи туда отбрасываются.
module opll2opl (
    input  wire       clk,
    input  wire       rst,
    input  wire       vrc7,       // 1: набор патчей VRC7, 6 каналов; 0: YM2413
    input  wire       wr,         // запись регистра OPLL (строб 1 такт)
    input  wire [7:0] addr,
    input  wire [7:0] data,
    output wire       busy,       // есть необработанные записи
    output wire       full,       // очередь входа полна — wr будет потерян
    output reg        out_valid,  // запись OPL2 готова, держится до out_ack
    output reg  [7:0] out_reg,
    output reg  [7:0] out_val,
    input  wire       out_ack
);
  // --------------------------------------------------------------------
  // Очередь входа. Из VGM записи идут по одной через секвенсор, но 6502
  // в NSF пишет напрямую и ждать не умеет: пока переписываются шесть
  // каналов после байта пользовательского патча (до 36 записей OPL2),
  // драйвер успевает выдать следующий байт.
  reg [15:0] q_mem [0:31];
  reg  [5:0] q_wr = 0;
  reg  [5:0] q_rd = 0;
  wire       q_empty = q_wr == q_rd;
  assign full = (q_wr - q_rd) == 6'd32;

  // --------------------------------------------------------------------
  // ПЗУ патчей: два набора по 19 инструментов (0 — пользовательский,
  // 1-15 зашитые, 16-18 ритм) по 8 байт в формате регистров 0x00-0x07.
  // Байты из libvgm: emu2413.c (default_inst[0]) и opll_vrc7tone.h.
  reg [7:0] rom [0:303];
  initial begin
    // YM2413
    //  0 user
    rom[0] = 8'h00; rom[1] = 8'h00; rom[2] = 8'h00; rom[3] = 8'h00; rom[4] = 8'h00; rom[5] = 8'h00; rom[6] = 8'h00; rom[7] = 8'h00;
    //  1 violin
    rom[8] = 8'h71; rom[9] = 8'h61; rom[10] = 8'h1e; rom[11] = 8'h17; rom[12] = 8'hd0; rom[13] = 8'h78; rom[14] = 8'h00; rom[15] = 8'h17;
    //  2 guitar
    rom[16] = 8'h13; rom[17] = 8'h41; rom[18] = 8'h1a; rom[19] = 8'h0d; rom[20] = 8'hd8; rom[21] = 8'hf7; rom[22] = 8'h23; rom[23] = 8'h13;
    //  3 piano
    rom[24] = 8'h13; rom[25] = 8'h01; rom[26] = 8'h99; rom[27] = 8'h00; rom[28] = 8'hf2; rom[29] = 8'hc4; rom[30] = 8'h21; rom[31] = 8'h23;
    //  4 flute
    rom[32] = 8'h11; rom[33] = 8'h61; rom[34] = 8'h0e; rom[35] = 8'h07; rom[36] = 8'h8d; rom[37] = 8'h64; rom[38] = 8'h70; rom[39] = 8'h27;
    //  5 clarinet
    rom[40] = 8'h32; rom[41] = 8'h21; rom[42] = 8'h1e; rom[43] = 8'h06; rom[44] = 8'he1; rom[45] = 8'h76; rom[46] = 8'h01; rom[47] = 8'h28;
    //  6 oboe
    rom[48] = 8'h31; rom[49] = 8'h22; rom[50] = 8'h16; rom[51] = 8'h05; rom[52] = 8'he0; rom[53] = 8'h71; rom[54] = 8'h00; rom[55] = 8'h18;
    //  7 trumpet
    rom[56] = 8'h21; rom[57] = 8'h61; rom[58] = 8'h1d; rom[59] = 8'h07; rom[60] = 8'h82; rom[61] = 8'h81; rom[62] = 8'h11; rom[63] = 8'h07;
    //  8 organ
    rom[64] = 8'h33; rom[65] = 8'h21; rom[66] = 8'h2d; rom[67] = 8'h13; rom[68] = 8'hb0; rom[69] = 8'h70; rom[70] = 8'h00; rom[71] = 8'h07;
    //  9 horn
    rom[72] = 8'h61; rom[73] = 8'h61; rom[74] = 8'h1b; rom[75] = 8'h06; rom[76] = 8'h64; rom[77] = 8'h65; rom[78] = 8'h10; rom[79] = 8'h17;
    // 10 synth
    rom[80] = 8'h41; rom[81] = 8'h61; rom[82] = 8'h0b; rom[83] = 8'h18; rom[84] = 8'h85; rom[85] = 8'hf0; rom[86] = 8'h81; rom[87] = 8'h07;
    // 11 harpsichord
    rom[88] = 8'h33; rom[89] = 8'h01; rom[90] = 8'h83; rom[91] = 8'h11; rom[92] = 8'hea; rom[93] = 8'hef; rom[94] = 8'h10; rom[95] = 8'h04;
    // 12 vibraphone
    rom[96] = 8'h17; rom[97] = 8'hc1; rom[98] = 8'h24; rom[99] = 8'h07; rom[100] = 8'hf8; rom[101] = 8'hf8; rom[102] = 8'h22; rom[103] = 8'h12;
    // 13 synth bass
    rom[104] = 8'h61; rom[105] = 8'h50; rom[106] = 8'h0c; rom[107] = 8'h05; rom[108] = 8'hd2; rom[109] = 8'hf5; rom[110] = 8'h40; rom[111] = 8'h42;
    // 14 acoustic bass
    rom[112] = 8'h01; rom[113] = 8'h01; rom[114] = 8'h55; rom[115] = 8'h03; rom[116] = 8'he9; rom[117] = 8'h90; rom[118] = 8'h03; rom[119] = 8'h02;
    // 15 electric guitar
    rom[120] = 8'h41; rom[121] = 8'h41; rom[122] = 8'h89; rom[123] = 8'h03; rom[124] = 8'hf1; rom[125] = 8'he4; rom[126] = 8'hc0; rom[127] = 8'h13;
    // 16 BD
    rom[128] = 8'h01; rom[129] = 8'h01; rom[130] = 8'h18; rom[131] = 8'h0f; rom[132] = 8'hdf; rom[133] = 8'hf8; rom[134] = 8'h6a; rom[135] = 8'h6d;
    // 17 HH/SD
    rom[136] = 8'h01; rom[137] = 8'h01; rom[138] = 8'h00; rom[139] = 8'h00; rom[140] = 8'hc8; rom[141] = 8'hd8; rom[142] = 8'ha7; rom[143] = 8'h68;
    // 18 TOM/CYM
    rom[144] = 8'h05; rom[145] = 8'h01; rom[146] = 8'h00; rom[147] = 8'h00; rom[148] = 8'hf8; rom[149] = 8'haa; rom[150] = 8'h59; rom[151] = 8'h55;
    // VRC7
    //  0 user
    rom[152] = 8'h00; rom[153] = 8'h00; rom[154] = 8'h00; rom[155] = 8'h00; rom[156] = 8'h00; rom[157] = 8'h00; rom[158] = 8'h00; rom[159] = 8'h00;
    //  1 buzzy bell
    rom[160] = 8'h03; rom[161] = 8'h21; rom[162] = 8'h05; rom[163] = 8'h06; rom[164] = 8'he8; rom[165] = 8'h81; rom[166] = 8'h42; rom[167] = 8'h27;
    //  2 guitar
    rom[168] = 8'h13; rom[169] = 8'h41; rom[170] = 8'h14; rom[171] = 8'h0d; rom[172] = 8'hd8; rom[173] = 8'hf6; rom[174] = 8'h23; rom[175] = 8'h12;
    //  3 wurly
    rom[176] = 8'h11; rom[177] = 8'h11; rom[178] = 8'h08; rom[179] = 8'h08; rom[180] = 8'hfa; rom[181] = 8'hb2; rom[182] = 8'h20; rom[183] = 8'h12;
    //  4 flute
    rom[184] = 8'h31; rom[185] = 8'h61; rom[186] = 8'h0c; rom[187] = 8'h07; rom[188] = 8'ha8; rom[189] = 8'h64; rom[190] = 8'h61; rom[191] = 8'h27;
    //  5 clarinet
    rom[192] = 8'h32; rom[193] = 8'h21; rom[194] = 8'h1e; rom[195] = 8'h06; rom[196] = 8'he1; rom[197] = 8'h76; rom[198] = 8'h01; rom[199] = 8'h28;
    //  6 synth
    rom[200] = 8'h02; rom[201] = 8'h01; rom[202] = 8'h06; rom[203] = 8'h00; rom[204] = 8'ha3; rom[205] = 8'he2; rom[206] = 8'hf4; rom[207] = 8'hf4;
    //  7 trumpet
    rom[208] = 8'h21; rom[209] = 8'h61; rom[210] = 8'h1d; rom[211] = 8'h07; rom[212] = 8'h82; rom[213] = 8'h81; rom[214] = 8'h11; rom[215] = 8'h07;
    //  8 organ
    rom[216] = 8'h23; rom[217] = 8'h21; rom[218] = 8'h22; rom[219] = 8'h17; rom[220] = 8'ha2; rom[221] = 8'h72; rom[222] = 8'h01; rom[223] = 8'h17;
    //  9 bells
    rom[224] = 8'h35; rom[225] = 8'h11; rom[226] = 8'h25; rom[227] = 8'h00; rom[228] = 8'h40; rom[229] = 8'h73; rom[230] = 8'h72; rom[231] = 8'h01;
    // 10 vibes
    rom[232] = 8'hb5; rom[233] = 8'h01; rom[234] = 8'h0f; rom[235] = 8'h0f; rom[236] = 8'ha8; rom[237] = 8'ha5; rom[238] = 8'h51; rom[239] = 8'h02;
    // 11 vibraphone
    rom[240] = 8'h17; rom[241] = 8'hc1; rom[242] = 8'h24; rom[243] = 8'h07; rom[244] = 8'hf8; rom[245] = 8'hf8; rom[246] = 8'h22; rom[247] = 8'h12;
    // 12 tutti
    rom[248] = 8'h71; rom[249] = 8'h23; rom[250] = 8'h11; rom[251] = 8'h06; rom[252] = 8'h65; rom[253] = 8'h74; rom[254] = 8'h18; rom[255] = 8'h16;
    // 13 fretless
    rom[256] = 8'h01; rom[257] = 8'h02; rom[258] = 8'hd3; rom[259] = 8'h05; rom[260] = 8'hc9; rom[261] = 8'h95; rom[262] = 8'h03; rom[263] = 8'h02;
    // 14 synth bass
    rom[264] = 8'h61; rom[265] = 8'h63; rom[266] = 8'h0c; rom[267] = 8'h00; rom[268] = 8'h94; rom[269] = 8'hc0; rom[270] = 8'h33; rom[271] = 8'hf6;
    // 15 sweep
    rom[272] = 8'h21; rom[273] = 8'h72; rom[274] = 8'h0d; rom[275] = 8'h00; rom[276] = 8'hc1; rom[277] = 8'hd5; rom[278] = 8'h56; rom[279] = 8'h06;
    // 16 BD
    rom[280] = 8'h01; rom[281] = 8'h01; rom[282] = 8'h18; rom[283] = 8'h0f; rom[284] = 8'hdf; rom[285] = 8'hf8; rom[286] = 8'h6a; rom[287] = 8'h6d;
    // 17 HH/SD
    rom[288] = 8'h01; rom[289] = 8'h01; rom[290] = 8'h00; rom[291] = 8'h00; rom[292] = 8'hc8; rom[293] = 8'hd8; rom[294] = 8'ha7; rom[295] = 8'h68;
    // 18 TOM/CYM
    rom[296] = 8'h05; rom[297] = 8'h01; rom[298] = 8'h00; rom[299] = 8'h00; rom[300] = 8'hf8; rom[301] = 8'haa; rom[302] = 8'h59; rom[303] = 8'h55;
  end
  reg [8:0] rom_addr = 0;
  reg [7:0] rom_q = 0;
  always @(posedge clk) rom_q <= rom[rom_addr];

  // --------------------------------------------------------------------
  // Состояние OPLL, которого хватает для перевода
  reg [7:0] user [0:7];       // регистры 0x00-0x07
  reg [3:0] inst [0:8];
  reg [3:0] vol  [0:8];
  reg [8:0] fnum [0:8];
  reg [2:0] blk  [0:8];
  reg [8:0] key = 0;
  reg [8:0] sus = 0;
  reg [8:0] loaded = 0;       // патч канала хоть раз уехал в OPL2
  reg [5:0] r0e = 0;          // регистр ритма YM2413
  reg [3:0] rvol_hh = 0;      // громкости модуляторов ритма: 0x37[7:4]
  reg [3:0] rvol_tom = 0;     // и 0x38[7:4]
  reg       inited = 0;       // WSE и глубины LFO записаны в OPL2

  wire       rhythm = !vrc7 && r0e[5];
  wire [3:0] nch = vrc7 ? 4'd6 : 4'd9;

  // Регистры OPL2 одного канала, перечислены индексом k:
  //   0 mod 0x20  1 car 0x20  2 mod 0x40  3 car 0x40  4 mod 0x60
  //   5 car 0x60  6 mod 0x80  7 car 0x80  8 mod 0xE0  9 car 0xE0
  //  10 ch 0xC0  11 ch 0xA0  12 ch 0xB0
  localparam [12:0] M_FREQ  = 13'b1_1000_0000_0000;
  localparam [12:0] M_KEY   = 13'b1_0000_1100_0000;
  localparam [12:0] M_PATCH = 13'b0_0111_1111_1111;
  localparam [12:0] M_ALL   = 13'b1_1111_1111_1111;

  // Какие регистры OPL2 зависят от байта пользовательского патча
  function automatic [12:0] user_mask(input [2:0] b);
    case (b)
      3'd0: user_mask = 13'b0_0000_0000_0001;
      3'd1: user_mask = 13'b0_0000_1000_0010;  // EG несущей влияет на RR
      3'd2: user_mask = 13'b0_0000_0000_0100;
      3'd3: user_mask = 13'b0_0111_0000_1000;  // KSL несущей, волны, FB
      3'd4: user_mask = 13'b0_0000_0001_0000;
      3'd5: user_mask = 13'b0_0000_0010_0000;
      3'd6: user_mask = 13'b0_0000_0100_0000;
      default: user_mask = 13'b0_0000_1000_0000;
    endcase
  endfunction

  // Номер ячейки модулятора канала в адресации OPL2
  function automatic [4:0] slot_of(input [3:0] c);
    case (c)
      4'd0: slot_of = 5'd0;  4'd1: slot_of = 5'd1;  4'd2: slot_of = 5'd2;
      4'd3: slot_of = 5'd8;  4'd4: slot_of = 5'd9;  4'd5: slot_of = 5'd10;
      4'd6: slot_of = 5'd16; 4'd7: slot_of = 5'd17; default: slot_of = 5'd18;
    endcase
  endfunction

  function automatic [1:0] ksl_of(input [1:0] kl);
    ksl_of = kl == 2'd1 ? 2'd2 : kl == 2'd2 ? 2'd1 : kl;
  endfunction

  // Байт патча, который нужен регистру k (и второй — только для k=7)
  function automatic [2:0] byte_a(input [3:0] kk);
    byte_a = kk < 4'd8 ? kk[2:0] : 3'd3;
  endfunction

  localparam S_IDLE = 4'd0;
  localparam S_INIT = 4'd1;
  localparam S_DEC  = 4'd2;
  localparam S_CH   = 4'd3;
  localparam S_K    = 4'd4;
  localparam S_WA   = 4'd5;
  localparam S_FB   = 4'd6;
  localparam S_WB   = 4'd7;
  localparam S_EMIT = 4'd8;
  localparam S_OUT  = 4'd9;

  reg [3:0]  st = S_IDLE;
  reg [7:0]  c_addr = 0;
  reg [7:0]  c_data = 0;
  reg [12:0] mask = 0;
  reg        multi = 0;       // обход всех каналов с пользовательским патчем
  reg        rhy_all = 0;     // обход каналов 6-8 при смене режима ритма
  reg [3:0]  ch = 0;
  reg [3:0]  k = 0;
  reg [1:0]  init_step = 0;
  reg        pre = 0;         // выдаётся запись-преамбула (0xBD), обход после неё
  reg [7:0]  pa = 0;

  wire       rhy_ch = rhythm && ch >= 4'd6;
  wire [4:0] inst5  = rhy_ch ? 5'd10 + {1'b0, ch} : {1'b0, inst[ch]};
  wire [8:0] rom_base = vrc7 ? 9'd152 : 9'd0;
  wire       from_user = !rhy_ch && inst[ch] == 4'd0;
  wire [4:0] smod = slot_of(ch);
  wire [4:0] scar = smod + 5'd3;
  wire [3:0] rvol = ch == 4'd7 ? rvol_hh : rvol_tom;
  wire       held = key[ch] || rhy_ch;   // клавиша считается нажатой

  // Канал участвует в обходе
  wire ch_sel = !multi || rhy_all || (inst[ch] == 4'd0 && !rhy_ch);
  wire ch_last = ch == nch - 1'b1 || (rhy_all && ch == 4'd8);

  assign busy = !q_empty || st != S_IDLE || !inited;

  integer i;
  always @(posedge clk) begin
    if (rst) begin
      q_wr <= 0;
      q_rd <= 0;
      st <= S_IDLE;
      out_valid <= 0;
      key <= 0;
      sus <= 0;
      loaded <= 0;
      r0e <= 0;
      rvol_hh <= 0;
      rvol_tom <= 0;
      inited <= 0;
      init_step <= 0;
      pre <= 0;
      for (i = 0; i < 8; i = i + 1) user[i] <= 8'd0;
      for (i = 0; i < 9; i = i + 1) begin
        inst[i] <= 4'd0;
        vol[i]  <= 4'd0;
        fnum[i] <= 9'd0;
        blk[i]  <= 3'd0;
      end
    end else begin
      if (wr && !full) begin
        q_mem[q_wr[4:0]] <= {addr, data};
        q_wr <= q_wr + 1'b1;
      end

      case (st)
        S_IDLE: begin
          if (!inited) begin
            st <= S_INIT;
          end else if (!q_empty) begin
            c_addr <= q_mem[q_rd[4:0]][15:8];
            c_data <= q_mem[q_rd[4:0]][7:0];
            q_rd <= q_rd + 1'b1;
            st <= S_DEC;
          end
        end

        // Однократно после сброса: включить выбор волны и глубокие
        // тремоло/вибрато — у OPLL они такие всегда
        S_INIT: begin
          case (init_step)
            2'd0: begin out_reg <= 8'h01; out_val <= 8'h20; end
            2'd1: begin out_reg <= 8'h08; out_val <= 8'h00; end
            default: begin out_reg <= 8'hBD; out_val <= 8'hC0; end
          endcase
          out_valid <= 1;
          st <= S_OUT;
        end

        S_DEC: begin
          multi <= 0;
          rhy_all <= 0;
          pre <= 0;
          ch <= c_addr[3:0];
          st <= S_CH;
          if (c_addr < 8'h08) begin
            user[c_addr[2:0]] <= c_data;
            mask <= user_mask(c_addr[2:0]);
            multi <= 1;
            ch <= 0;
          end else if (c_addr == 8'h0E) begin
            if (vrc7) st <= S_IDLE;
            else begin
              r0e <= c_data[5:0];
              out_reg <= 8'hBD;
              out_val <= {2'b11, c_data[5:0]};
              out_valid <= 1;
              // смена режима — у каналов 6-8 меняется источник патча
              pre <= 1;
              st <= S_OUT;
              if (c_data[5] != r0e[5]) begin
                mask <= M_ALL;
                multi <= 1;
                rhy_all <= 1;
                ch <= 4'd6;
              end else mask <= 0;
            end
          end else if (c_addr[7:4] == 4'h1 && c_addr[3:0] < nch) begin
            fnum[c_addr[3:0]][7:0] <= c_data;
            mask <= M_FREQ;
          end else if (c_addr[7:4] == 4'h2 && c_addr[3:0] < nch) begin
            fnum[c_addr[3:0]][8] <= c_data[0];
            blk[c_addr[3:0]] <= c_data[3:1];
            key[c_addr[3:0]] <= c_data[4];
            sus[c_addr[3:0]] <= c_data[5];
            // первое нажатие канала, чей патч ещё не уезжал: OPL2 хранит
            // нули, и нота вышла бы молчаливой либо чужой
            if (c_data[4] && !loaded[c_addr[3:0]]) begin
              loaded[c_addr[3:0]] <= 1;
              mask <= M_ALL;
            end else begin
              mask <= M_KEY;
            end
          end else if (c_addr[7:4] == 4'h3 && c_addr[3:0] < nch) begin
            vol[c_addr[3:0]] <= c_data[3:0];
            if (rhythm && c_addr[3:0] >= 4'd6) begin
              // в режиме ритма старший ниббл 0x37/0x38 — громкость
              // хай-хэта и тома, инструмент не меняется
              if (c_addr[3:0] == 4'd7) rvol_hh <= c_data[7:4];
              if (c_addr[3:0] == 4'd8) rvol_tom <= c_data[7:4];
              mask <= c_addr[3:0] == 4'd6 ? 13'b0_0000_0000_1000
                                          : 13'b0_0000_0000_1100;
            end else begin
              inst[c_addr[3:0]] <= c_data[7:4];
              loaded[c_addr[3:0]] <= 1;
              mask <= M_PATCH;
            end
          end else begin
            st <= S_IDLE;
          end
        end

        // Обход каналов: пропустить те, что не участвуют
        S_CH: begin
          if (mask == 0) st <= S_IDLE;
          else if (ch_sel) begin
            k <= 0;
            st <= S_K;
          end else if (ch_last) st <= S_IDLE;
          else ch <= ch + 1'b1;
        end

        // Обход регистров канала по маске
        S_K: begin
          if (k > 4'd12) begin
            if (multi && !ch_last) begin
              ch <= ch + 1'b1;
              st <= S_CH;
            end else st <= S_IDLE;
          end else if (mask[k]) begin
            rom_addr <= rom_base + {inst5, 3'b0} + {6'b0, byte_a(k)};
            st <= S_WA;
          end else k <= k + 1'b1;
        end

        S_WA: st <= S_FB;

        S_FB: begin
          pa <= from_user ? user[byte_a(k)] : rom_q;
          rom_addr <= rom_base + {inst5, 3'b0} + 9'd1;
          st <= S_WB;
        end

        S_WB: st <= S_EMIT;

        S_EMIT: begin
          out_valid <= 1;
          st <= S_OUT;
          case (k)
            4'd0: begin out_reg <= 8'h20 + {3'b0, smod}; out_val <= pa; end
            4'd1: begin out_reg <= 8'h20 + {3'b0, scar}; out_val <= pa; end
            4'd2: begin
              out_reg <= 8'h40 + {3'b0, smod};
              out_val <= {ksl_of(pa[7:6]),
                          (rhy_ch && ch != 4'd6) ? {rvol, 2'b00} : pa[5:0]};
            end
            4'd3: begin
              out_reg <= 8'h40 + {3'b0, scar};
              out_val <= {ksl_of(pa[7:6]), vol[ch], 2'b00};
            end
            4'd4: begin out_reg <= 8'h60 + {3'b0, smod}; out_val <= pa; end
            4'd5: begin out_reg <= 8'h60 + {3'b0, scar}; out_val <= pa; end
            // модулятор при снятой клавише замирает: RR = 0
            4'd6: begin
              out_reg <= 8'h80 + {3'b0, smod};
              out_val <= {pa[7:4], held ? pa[3:0] : 4'd0};
            end
            // несущая: 5 при sustain, RR при EG=1, иначе 7
            4'd7: begin
              out_reg <= 8'h80 + {3'b0, scar};
              out_val <= {pa[7:4],
                          held ? pa[3:0] :
                          sus[ch] ? 4'd5 :
                          (from_user ? user[1][5] : rom_q[5]) ? pa[3:0] : 4'd7};
            end
            4'd8: begin out_reg <= 8'hE0 + {3'b0, smod}; out_val <= {7'b0, pa[3]}; end
            4'd9: begin out_reg <= 8'hE0 + {3'b0, scar}; out_val <= {7'b0, pa[4]}; end
            4'd10: begin
              out_reg <= 8'hC0 + {4'b0, ch};
              out_val <= {2'b00, 2'b11, pa[2:0], 1'b0};
            end
            4'd11: begin
              out_reg <= 8'hA0 + {4'b0, ch};
              out_val <= {fnum[ch][6:0], 1'b0};
            end
            default: begin
              out_reg <= 8'hB0 + {4'b0, ch};
              out_val <= {2'b00, key[ch] && !rhy_ch, blk[ch], fnum[ch][8:7]};
            end
          endcase
        end

        S_OUT: begin
          if (out_ack) begin
            out_valid <= 0;
            if (!inited) begin
              if (init_step == 2'd2) begin
                inited <= 1;
                st <= S_IDLE;
              end else begin
                init_step <= init_step + 1'b1;
                st <= S_INIT;
              end
            end else if (pre) begin
              pre <= 0;
              st <= S_CH;
            end else begin
              k <= k + 1'b1;
              st <= S_K;
            end
          end
        end

        default: st <= S_IDLE;
      endcase
    end
  end
endmodule
