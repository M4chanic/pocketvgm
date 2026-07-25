// gbsbox — GBS-плеер: SM83 (VerilogBoy cpu) + GB APU (VerilogBoy sound)
// в собственном клоковом домене gb_clk (~4.19 МГц, регистровый клок из
// chipbox). Карта памяти плеера:
//   $0000-$03FF стаб-ROM (1 КБ BRAM, пишется из домена clk_sys — вторая
//               порт-сторона), GBS-код грузится с $0400+
//   $0000-$7FFF GBS-данные из PSRAM (банк 1 переключается записью
//               $2000-$3FFF, как MBC); стаб перекрывает $0000-$03FF
//   $A000-$BFFF cart-RAM 8 КБ (BRAM)
//   $C000-$DFFF WRAM 8 КБ (BRAM), $E000- зеркало
//   $FF10-$FF3F APU (sound.v декодирует сам)
//   $FF80-$FFFE HRAM
//   $FEA0 чтение: бит 0 = play-тик ожидает; запись: сброс
// ROM-чтения уходят в chipbox (домен clk_sys) как quasi-static запрос:
// адрес меняется не чаще раза за м-цикл (~950 нс), данные возвращаются
// задолго до фазы чтения CPU.

module gbsbox (
    input wire gb_clk,
    input wire rst,

    // запись стаба из clk_sys (истинная двухпортовая BRAM)
    input wire sys_clk,
    input wire stub_wr,
    input wire [14:0] stub_wr_addr,
    input wire [7:0] stub_wr_data,

    // play-тик (toggle из clk_sys, синхронизируется здесь)
    input wire play_tick_toggle,

    // ROM-запросы к PSRAM (quasi-static, домен gb_clk -> clk_sys)
    output reg [22:0] rom_addr = 0,
    output reg rom_req_toggle = 0,
    input wire [7:0] rom_data,

    output wire [15:0] snd_left,
    output wire [15:0] snd_right,

    // диагностика (тогглы gb-домена, синхронизируются снаружи):
    // тик доставлен в мейлбокс / SM83 записал звуковой регистр
    output reg tick_seen_toggle = 0,
    output reg sndwr_toggle = 0
);

  // ------------------------------------------------------------------
  // CPU
  wire [15:0] cpu_a;
  wire [7:0] cpu_dout;
  reg [7:0] cpu_din;
  wire cpu_rd;
  wire cpu_wr;

  vb_cpu sm83 (
      .clk(gb_clk),
      .rst(rst),
      .phi(),
      .ct(),
      .a(cpu_a),
      .dout(cpu_dout),
      .din(cpu_din),
      .rd(cpu_rd),
      .wr(cpu_wr),
      .int_en(int_ie),
      .int_flags_in(int_if),
      .int_flags_out(int_flags_out),
      .key_in(8'h00),
      .done(),
      .fault()
  );

  // ------------------------------------------------------------------
  // APU
  wire [7:0] snd_dout;

  sound apu (
      .clk(gb_clk),
      .rst(rst),
      .a(cpu_a),
      .dout(snd_dout),
      .din(cpu_dout),
      .rd(cpu_rd),
      .wr(cpu_wr),
      .left(snd_left),
      .right(snd_right),
      .ch1_level(),
      .ch2_level(),
      .ch3_level(),
      .ch4_level()
  );

  // ------------------------------------------------------------------
  // Память
  // ROM целиком в BRAM: $0000-$7FFF (стаб в младшем килобайте, тело
  // рипа с его load-адреса). Раньше тело читалось из PSRAM по запросу,
  // а SM83 брал rom_data уже на следующем такте gb_clk, ничего не
  // дожидаясь. На 1 МГц (только симулятор) круговой путь успевал, на
  // штатных 4.19 МГц — нет, и процессор исполнял мусор: отсюда молчание
  // на железе при растущем счётчике фетчей. Из BRAM байт готов за такт.
  reg [7:0] rom_ram[32768];
  reg [7:0] wram[8192];
  reg [7:0] cram[8192];
  reg [7:0] hram[127];

  always @(posedge sys_clk) begin
    if (stub_wr) rom_ram[stub_wr_addr] <= stub_wr_data;
  end

  // банк ROM (MBC-подобный)
  reg [7:0] rom_bank = 1;

  // Фейковые таймер и растр: GBS-драйверы поллят DIV ($FF04) и LY ($FF44)
  // в ожидании времени — вечные 0xFF вешают INIT (Metal Masters и др.)
  reg [13:0] div_pre = 0;   // DIV: инкремент каждые 256 тактов (16384 Гц)
  reg [7:0] div_reg = 0;
  reg [8:0] ly_pre = 0;     // LY: строка каждые 456 тактов, 0..153
  reg [7:0] ly_reg = 0;

  // Бут-инъекция: SM83 стартует с PC=$0000, но $0000-$003F по GBS-спеке
  // принадлежат RST-трамплинам (JP LOAD+n). На первом проходе после
  // сброса железо подменяет $0000-2 на «JP $00A0» (тело стаба); после
  // того как PC дошёл до $00A0, байты отдаются из стаб-BRAM как есть.
  reg booted = 0;

  // play-тик
  reg [2:0] tick_sync = 0;
  reg tick_pending = 0;

  // Прерывания: IE ($FFFF), IF ($FF0F). vb_cpu сам вектрит и чистит
  // обслуженный бит через int_flags_out. GBDK-рипы делают HALT в INIT,
  // ожидая vblank — раздаём его по растру (ly входит в 144). HALT будит
  // (IF&IE)!=0 даже без IME; поллинг-стаб держит DI, для него безвредно.
  reg [4:0] int_ie = 0;
  reg [4:0] int_if = 0;
  wire [4:0] int_flags_out;
  wire [4:0] vblank_pulse = (ly_pre == 9'd455 && ly_reg == 8'd143) ? 5'b00001 : 5'b0;

  always @(posedge gb_clk) begin
    if (rst) begin
      int_ie <= 0;
      int_if <= 0;
    end else begin
      int_if <= int_flags_out | vblank_pulse;
      if (cpu_wr && cpu_a == 16'hFFFF) int_ie <= cpu_dout[4:0];
      if (cpu_wr && cpu_a == 16'hFF0F) int_if <= cpu_dout[4:0] | vblank_pulse;
    end
  end

  // ROM-адрес到 PSRAM: $0000-$3FFF банк 0, $4000-$7FFF банк N
  wire [22:0] rom_lin = cpu_a[14]
      ? {1'b0, rom_bank, cpu_a[13:0]}
      : {8'b0, cpu_a[14:0]};

  reg [7:0] rom_q, wram_q, cram_q, hram_q;
  reg rom_hit_d = 0;
  reg [15:0] a_d;

  always @(posedge gb_clk) begin
    div_pre <= div_pre + 1'b1;
    if (div_pre[7:0] == 8'hFF) div_reg <= div_reg + 1'b1;
    ly_pre <= ly_pre + 1'b1;
    if (ly_pre == 9'd455) begin
      ly_pre <= 0;
      ly_reg <= ly_reg == 8'd153 ? 8'd0 : ly_reg + 1'b1;
    end

    tick_sync <= {tick_sync[1:0], play_tick_toggle};
    if (tick_sync[2] != tick_sync[1]) begin
      tick_pending <= 1;
      tick_seen_toggle <= ~tick_seen_toggle;
    end
    if (cpu_wr && cpu_a >= 16'hFF10 && cpu_a < 16'hFF40) sndwr_toggle <= ~sndwr_toggle;

    // защёлкиваем адрес и BRAM-чтения каждый такт
    a_d <= cpu_a;
    rom_q <= rom_ram[cpu_a[14:0]];
    // банк 1 ложится на те же адреса, что и линейный файл; всё выше
    // остаётся на старом пути через PSRAM
    rom_hit_d <= !cpu_a[14] || rom_bank == 8'd1;
    wram_q <= wram[cpu_a[12:0]];
    cram_q <= cram[cpu_a[12:0]];
    hram_q <= hram[cpu_a[6:0]];

    // ROM-запрос в PSRAM нужен только для банков выше первого: всё
    // остальное лежит в BRAM и читается за такт. Раньше запрос слался
    // на каждый адрес, и шина PSRAM грелась впустую.
    if (!cpu_a[15] && cpu_a != a_d && cpu_a[14] && rom_bank != 8'd1) begin
      rom_addr <= rom_lin;
      rom_req_toggle <= ~rom_req_toggle;
    end

    if (a_d == 16'h00A0) booted <= 1;

    if (rst) begin
      rom_bank <= 1;
      tick_pending <= 0;
      booted <= 0;
    end else if (cpu_wr) begin
      if (cpu_a[15:13] == 3'b001) rom_bank <= cpu_dout;  // $2000-$3FFF
      else if (cpu_a[15:13] == 3'b101) cram[cpu_a[12:0]] <= cpu_dout;  // $A000
      else if (cpu_a[15:13] == 3'b110) wram[cpu_a[12:0]] <= cpu_dout;  // $C000
      else if (cpu_a == 16'hFEA0) tick_pending <= 0;
      else if (cpu_a == 16'hFF04) div_reg <= 0;
      else if (cpu_a >= 16'hFF80 && cpu_a < 16'hFFFF) hram[cpu_a[6:0]] <= cpu_dout;
    end
  end

  // din-мультиплексор (по защёлкнутому адресу — BRAM-данные готовы)
  wire [7:0] boot_jp = a_d[1:0] == 2'd0 ? 8'hC3 : a_d[1:0] == 2'd1 ? 8'hA0 : 8'h00;

  always @(*) begin
    if (!a_d[15]) cpu_din = !booted && a_d < 16'h0003 ? boot_jp
        : rom_hit_d ? rom_q : rom_data;
    else if (a_d[15:13] == 3'b101) cpu_din = cram_q;
    else if (a_d[15:13] == 3'b110) cpu_din = wram_q;
    else if (a_d == 16'hFEA0) cpu_din = {7'b0, tick_pending};
    else if (a_d == 16'hFF04) cpu_din = div_reg;
    else if (a_d == 16'hFF44) cpu_din = ly_reg;
    else if (a_d == 16'hFF41) cpu_din = {6'b0, ly_reg >= 8'd144, 1'b0}; // STAT: mode1 в vblank
    else if (a_d == 16'hFF0F) cpu_din = {3'b111, int_if};  // IF (верхние биты = 1)
    else if (a_d == 16'hFFFF) cpu_din = {3'b111, int_ie};  // IE
    else if (a_d >= 16'hFF10 && a_d < 16'hFF40) cpu_din = snd_dout;
    else if (a_d >= 16'hFF80 && a_d < 16'hFFFF) cpu_din = hram_q;
    else cpu_din = 8'hFF;
  end

endmodule
