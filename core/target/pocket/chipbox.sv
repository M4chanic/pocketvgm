// chipbox — звуковые чипы m4pocket + аппаратный секвенсор VGM-команд.
//
// Wishbone-слейв на внешнем регионе LiteX (0x8000_0000). Фирмварь заливает
// в командный FIFO поток слов:
//   [31:28] = 0x1 — запись в YM2151: [15:8] регистр, [7:0] значение
//   [31:28] = 0x2 — запись в AY-3-8910/YM2149: [11:8] регистр, [7:0] значение
//   [31:28] = 0x3 — запись в SegaPCM: [15:8] регистр, [7:0] значение
//   [31:28] = 0x4 — запись в MSM6258: [9:8] регистр (0 упр., 1 данные,
//                   2 пан: бит0 глушит левый, бит1 правый), [7:0] значение
//   [31:28] = 0x5 — адрес ADPCM-потока в памяти сэмплов, байты [22:0]
//   [31:28] = 0x6 — длина ADPCM-потока [23:0] байт + активация префетчера
//   [31:28] = 0x7 — остановить префетчер ADPCM-потока
//   [31:28] = 0x8 — ждать [23:0] тиков по 1/44100 с
//   [31:28] = 0x9 — запись в NES APU: [12:8] регистр ($4000+r), [7:0] значение
//   [31:28] = 0xA — указатель записи NES-RAM (DPCM), байты [14:0]
//                   окна $8000-$FFFF
//   [31:28] = 0xC — запись в OPL3: [16:8] = {банк, регистр}, [7:0] значение
//   [31:28] = 0xD — запись в YM2612: [16] порт, [15:8] регистр, [7:0] значение
//   [31:28] = 0xE — запись в SN76489: [7:0] значение
//   [31:28] = 0xB — запись байта [7:0] DPCM-данных по указателю (авто-инкремент);
//                   идёт через FIFO, т.е. синхронно с потоком команд —
//                   VGM переключает DPCM-страницы посреди трека
// FSM исполняет их с точным временем: тики считает дробный делитель от
// clk_sys, записи в YM2151 ждут снятия busy-флага (бит 7 статуса jt51).
//
// Карта регистров (словные смещения от 0x8000_0000):
//   0x0 W  — push команды в FIFO
//   0x1 R  — статус: [12:0] занято слов FIFO, [29] секвенсор занят,
//            [30] FIFO пуст, [31] FIFO полон
//   0x2 W  — управление: бит 0 — сброс (чипы + FIFO + секвенсор)
//   0x3 RW — фазовый инкремент clock-enable YM2151:
//            inc = Fchip / CLK_HZ * 2^32 (по умолчанию 3.579545 МГц)
//   0x4 RW — фазовый инкремент clock-enable AY (по умолчанию 1.789773 МГц)
//   0x5 RW — фазовый инкремент clock-enable SegaPCM; cen = 2 x клок чипа
//            из VGM-заголовка (по умолчанию 8 МГц)
//   0x6 RW — коэффициенты микса, 1/64 доли: [7:0] YM, [15:8] AY,
//            [23:16] SegaPCM, [31:24] MSM6258 (по умолчанию все 64 = 1.0)
//   0x7 RW — фазовый инкремент clock-enable MSM6258 (по умолчанию 8 МГц)
//   0xA RW — конфиг MSM6258: [1:0] делитель клока из VGM-флагов
//            (0 -> /1024, 1 -> /768, 2/3 -> /512)
//   0xB RW — фазовый инкремент клока NES APU (по умолчанию 1.789773 МГц)
//   0xC RW — коэффициенты микса 2: [7:0] NES APU (по умолчанию 64)
//   0xD W  — запись байта стаб-ROM NSF ($5000-$50FF): [15:8] адрес, [7:0] байт
//   0xE W  — запись байта векторов $FFFA-$FFFF: [10:8] индекс 0-5, [7:0] байт
//   0xF RW — фазовый инкремент play-тика NSF/GBS (Fplay / CLK_HZ * 2^32)
//   0x10 RW — фазовый инкремент полуклока GB (2 x 4.194304 МГц по умолчанию)
//   0x11 W  — запись байта стаб-ROM GBS ($0000-$03FF): [17:8] адрес, [7:0] байт
//   0x14 RW — фазовый инкремент полуклока OPL3 (2 x 12.727 МГц по умолчанию)
//   0x15 RW — коэффициенты микса 3: [7:0] YM2612, [15:8] SN76489
//   0x16 RW — фазовый инкремент clock-enable YM2612 (7.670453 МГц)
//   0x17 RW — фазовый инкремент clock-enable SN76489 (3.579545 МГц)
//
// Режим GBS (бит 3 регистра 0x2, запуск — бит 2, как NSF): SM83 + GB APU
// (gbsbox, свой клоковый домен ~4.19 МГц); GBS-данные в том же регионе
// PSRAM, что и NSF (форматы взаимоисключающие). Гейн GB — рег 0xC[15:8].
//
// Режим NSF (бит 1 регистра управления 0x2): вместо VGM-секвенсора музыку
// исполняет 6502 (ядро Arlet) — карта памяти NES-плеера:
//   $0000-$07FF RAM (BRAM), $4000-$401F APU, $5000-$50FF стаб-ROM,
//   $5FF0 play-тик (чтение: бит 0 = ожидает; запись: сброс),
//   $5FF8-$5FFF банки 4 КБ (сброс — identity), $6000-$7FFF WRAM (BRAM),
//   $8000-$FFFF NSF-данные из PSRAM через банки ($FFFA-$FFFF — теневые
//   векторы). DMC в этом режиме тоже читает через банки.
// Бит 2 регистра 0x2 — запуск CPU (0 = держать в сбросе).
//
// Карта памяти сэмплов (PSRAM, байтовые адреса):
//   0x000000+ ROM SegaPCM; 0x400000+ банк ADPCM-потоков MSM6258;
//   0x600000+ окно NES-RAM $8000-$FFFF (32 КБ, DPCM-сэмплы DMC)
//   0x8 W  — адрес загрузки сэмпл-ROM (байтовый, авто-инкремент +2)
//   0x9 W  — 16 бит данных сэмпл-ROM (little-endian), пишется по адресу
//            из 0x8; ack удерживается, пока память занята
// --------------------------------------------------------------------------
// Варианты битстрима (APF позволяет до 8 .rev в одном пакете ядра, выбор
// пользователем через variants.json). Кристалл не вмещает все чипы разом:
//   M4_SIM     — всё сразу (симуляция и селфтесты)
//   M4_ARCADE  — аркадный вариант: YM2151, SegaPCM, MSM6258, OKIM6295,
//                K053260 (+ общие YM2612/SN76489/OPL3). Сюда же X68000.
//   (по умолчанию) — домашний: MD, NES, AY, SID, Game Boy, OPL3, SCC
// Дальше в коде гейтим по производным M4_HAS_HOME / M4_HAS_ARCADE.
`ifdef M4_SIM
  `define M4_HAS_HOME
  `define M4_HAS_ARCADE
`elsif M4_ARCADE
  `define M4_HAS_ARCADE
`else
  `define M4_HAS_HOME
`endif

module chipbox #(
    parameter CLK_HZ = 57_120_000
) (
    input wire clk,
    input wire reset,

    // Wishbone (classic), как в wishbone.sv
    input wire [29:0] addr,
    input wire [1:0] bte,
    input wire [2:0] cti,
    input wire cyc,
    input wire [31:0] data_write,
    input wire [3:0] sel,
    input wire stb,
    input wire we,
    output reg ack = 0,  // импульс; удержание шины см. wb_served
    output reg [31:0] data_read = 0,
    output reg err = 0,

    // Последний сэмпл чипов (домен clk). Тоггл меняется на каждом новом
    // сэмпле — для синхронизации в аудио-домен.
    output reg signed [15:0] chip_left = 0,
    output reg signed [15:0] chip_right = 0,
    output reg chip_sample_toggle = 0,

    // Память сэмплов (PSRAM в core_top, C++-модель в Verilator).
    // Адрес — в 16-битных словах; строб на один такт, ответ чтения —
    // mem_rdata_valid с данными на mem_rdata.
    output reg mem_rd = 0,
    output reg mem_wr = 0,
    output reg [21:0] mem_addr = 0,
    output reg [15:0] mem_wdata = 0,
    output reg [1:0] mem_wbe = 2'b11,
    input wire [15:0] mem_rdata,
    input wire mem_rdata_valid,
    input wire mem_busy,
    // {счётчик обновлений, id последнего обновлённого слота} от APF
    input wire [23:0] slot_upd_info
`ifdef M4_SIM
    ,
    // Отладка (только для Verilator-сборок)
    output wire [15:0] dbg_cpu_ab,
    output wire dbg_cpu_step,
    output wire dbg_cpu_we,
    output wire [7:0] dbg_cpu_di,
    output wire [7:0] dbg_cpu_do,
    output wire dbg_apu_cs,
    output wire [7:0] dbg_apu_din,
    output wire [4:0] dbg_apu_addr,
    output wire dbg_phi2,
    output wire [7:0] dbg_strobes,
    output wire dbg_sid_cs,
    output wire dbg_sid_we,
    output wire [4:0] dbg_sid_addr,
    output wire [7:0] dbg_sid_din,
    output wire [17:0] dbg_sid_audio,
    output wire [7:0] dbg_cen_sid_cnt,
    output wire [22:0] dbg_gbs_rom_addr,
    output wire dbg_gbs_rom_toggle
`endif
);

  localparam [31:0] DEFAULT_PHASE_INC = 32'((64'd3_579_545 << 32) / CLK_HZ);
  localparam [31:0] DEFAULT_AY_PHASE_INC = 32'((64'd1_789_773 << 32) / CLK_HZ);
  localparam [31:0] DEFAULT_PCM_PHASE_INC = 32'((64'd8_000_000 << 32) / CLK_HZ);
  localparam [31:0] DEFAULT_ADPCM_PHASE_INC = 32'((64'd8_000_000 << 32) / CLK_HZ);
  localparam [31:0] DEFAULT_NES_PHASE_INC = 32'((64'd1_789_773 << 32) / CLK_HZ);
  localparam [21:0] NES_BASE_WORD = 22'h300000;  // 0x600000 байт
  localparam [21:0] NSF_BASE_WORD = 22'h380000;  // 0x700000 байт, до 1 МБ

  // --------------------------------------------------------------------
  // Командный FIFO (один клоковый домен)
  localparam FIFO_AW = 12;  // 4096 слов (DPCM-страницы идут через FIFO)

  reg [31:0] fifo_mem[2**FIFO_AW];
  reg [FIFO_AW:0] wr_ptr = 0;
  reg [FIFO_AW:0] rd_ptr = 0;

  wire [FIFO_AW:0] fifo_used = wr_ptr - rd_ptr;
  wire fifo_empty = wr_ptr == rd_ptr;
  wire fifo_full = fifo_used[FIFO_AW];

  // --------------------------------------------------------------------
  // Wishbone
  reg soft_reset_req = 0;
  // Текущая транзакция шины уже обслужена; снимается, когда мастер
  // отпускает stb/cyc. Без этого блок исполнялся повторно всё время
  // удержания шины — подробности у него самого.
  reg wb_served = 0;
  reg [31:0] phase_inc = DEFAULT_PHASE_INC;
  reg [31:0] ay_phase_inc = DEFAULT_AY_PHASE_INC;
  reg [31:0] pcm_phase_inc = DEFAULT_PCM_PHASE_INC;
  reg [31:0] mix_gains = {8'd64, 8'd64, 8'd64, 8'd64};  // {adpcm, pcm, ay, ym}
  reg [31:0] adpcm_phase_inc = DEFAULT_ADPCM_PHASE_INC;
  reg [1:0] adpcm_clkdiv = 2'd2;  // /512
  reg [31:0] nes_phase_inc = DEFAULT_NES_PHASE_INC;
  reg [7:0] apu_gain = 8'd64;
  reg [7:0] gb_gain = 8'd64;
`ifdef M4_HAS_HOME
  reg [31:0] scc_phase_inc = DEFAULT_SCC_PHASE_INC;
  reg [7:0] scc_gain = 8'd64;
  reg [31:0] huc_phase_inc = DEFAULT_HUC_PHASE_INC;
  reg [7:0] huc_gain = 8'd64;
`endif
`ifdef M4_HAS_ARCADE
  reg [31:0] okim_phase_inc = DEFAULT_OKIM_PHASE_INC;
  reg [7:0] okim_gain = 8'd64;
  reg okim_ss = 1;
  reg [31:0] k060_phase_inc = DEFAULT_K060_PHASE_INC;
  reg [7:0] k060_gain = 8'd64;
`endif
  reg nsf_mode = 0;
  reg gbs_mode = 0;
  // Режим прямой записи в APU Game Boy (VGM): CPU и ROM не нужны, но
  // сам APU обязан выйти из сброса, иначе записи уходят в никуда.
  reg gb_apu_mode = 0;
  reg cpu_run = 0;
  reg [31:0] play_phase_inc = 0;
  reg stub_wr = 0;
  reg [7:0] stub_wr_addr = 0;
  reg [7:0] stub_wr_data = 0;
  reg [7:0] vector_regs[6];
  localparam [31:0] DEFAULT_SID_PHASE_INC = 32'((64'd985_248 << 32) / CLK_HZ);
  reg [31:0] sid_phase_inc = DEFAULT_SID_PHASE_INC;
  reg sid_v8580 = 0;
  reg [7:0] sid_gain = 8'd64;
  reg sid_mode = 0;
  // Пауза (контрол бит 5): замораживает cen всех чипов, тик секвенсора,
  // play-тик и регистровые клоки gb/opl. Звук уходит в тишину DC-блокерами.
  reg pause_r = 0 /* synthesis maxfan = 200 */;
  // Перемотка (контрол бит 6): тик секвенсора и play-тик в 8 раз быстрее,
  // чипы на номинальной скорости — классический fast-forward
  reg ff_r = 0;
  // Диагностика PSRAM-путей («недостающие звуки»): счётчики фетчей
  // ADPCM-потока и DMC DMA, чистятся софт-сбросом, читаются с 5'h19
  reg [15:0] pf_cnt = 0;
  reg [15:0] dmc_cnt = 0;
  // Отладочное чтение PSRAM фирмварью: запись 0x1F = байтовый адрес
  // (тоггл-запрос в арбитр), чтение 0x1F = {valid, байт}
  reg [22:0] dbg_rd_addr = 0;
  reg dbg_req_t = 0;
  reg dbg_req_t_d = 0;
  reg dbg_rd_pend = 0;
  reg dbg_wait = 0;
  reg dbg_lane = 0;
  reg [7:0] dbg_rd_data = 0;
  reg dbg_rd_valid = 0;
  // Фетчи CPU из PSRAM: nsf_fetch (6502 NSF/SID), gbs_fetch (SM83 ROM);
  // чтение WB 0x1D, чистка софт-сбросом (в always арбитра)
  reg [15:0] nsf_fetch = 0;
  reg [15:0] gbs_fetch = 0;
  // Диагностика CPU-форматов на железе: p_acks — стаб обслужил play-тик
  // (запись $5FF0/$D7F0), snd_wr — записи CPU в звуковые реги (APU/5B/SID);
  // чтение WB 0x1B, чистятся софт-сбросом
  reg [15:0] p_acks = 0;
  reg [15:0] snd_wr = 0;
  // VRC6 (контрол бит 7): экспаншен-чипы NSF, записи $9xxx-$Bxxx от 6502
  reg vrc6_en = 0;
  reg vrc6_wr = 0;
  reg [1:0] vrc6_blk = 0;
  reg [1:0] vrc6_rsel = 0;
  reg [7:0] vrc6_din = 0;
  // «Пульс» доменов GB/OPL: если регистровый клок домена мёртв на железе,
  // его выходной сэмпл не меняется — счётчики смен покажут это с экрана
  reg [15:0] gb_beat = 0;
  reg [15:0] opl_beat = 0;
  reg signed [15:0] gb_beat_prev = 0;
  reg signed [15:0] opl_beat_prev = 0;
  // VU-метр: пики |L|/|R| выхода микса, чтение 5'h1A очищает
  // (тоггл-хендшейк vu_take между WB-блоком и блоком микса, домен один)
  reg [15:0] vu_l = 0;
  reg [15:0] vu_r = 0;
  reg vu_take = 0;
  reg vu_take_d = 0;
  localparam [31:0] DEFAULT_OPL_PHASE_INC = 32'((64'd25_454_000 << 32) / CLK_HZ);
  localparam [31:0] DEFAULT_FM_PHASE_INC = 32'((64'd7_670_453 << 32) / CLK_HZ);
  localparam [31:0] DEFAULT_SN_PHASE_INC = 32'((64'd3_579_545 << 32) / CLK_HZ);
  reg [31:0] fm_phase_inc = DEFAULT_FM_PHASE_INC;
  reg [31:0] sn_phase_inc = DEFAULT_SN_PHASE_INC;
  reg [7:0] fm_gain = 8'd64;
  reg [7:0] sn_gain = 8'd32;
  reg [31:0] opl_phase_inc = DEFAULT_OPL_PHASE_INC;
  reg [7:0] opl_gain = 8'd64;
  localparam [31:0] DEFAULT_GB_PHASE_INC = 32'((64'd8_388_608 << 32) / CLK_HZ);
  // Konami SCC (K051649): phiM ~1.79 МГц
  localparam [31:0] DEFAULT_SCC_PHASE_INC = 32'((64'd1_789_772 << 32) / CLK_HZ);
  localparam [31:0] DEFAULT_HUC_PHASE_INC = 32'((64'd3_579_545 << 32) / CLK_HZ);
  // OKIM6295: клок ~1 МГц; ROM сэмплов в PSRAM с байта 0x100000 (окно
  // 0x100000-0x3FFFFF свободно между SegaPCM<=512К и ADPCM_BASE 0x400000)
  localparam [31:0] DEFAULT_OKIM_PHASE_INC = 32'((64'd1_000_000 << 32) / CLK_HZ);
  localparam [21:0] OKIM_BASE_WORD = 22'h08_0000;  // байт 0x100000
  // K053260 (Konami): клок ~3.58 МГц; ROM сэмплов (до 2 МБ, 21-битный
  // адрес) в PSRAM с байта 0x200000 (окно 0x200000-0x3FFFFF свободно)
  localparam [31:0] DEFAULT_K060_PHASE_INC = 32'((64'd3_579_545 << 32) / CLK_HZ);
  localparam [21:0] K060_BASE_WORD = 22'h10_0000;  // байт 0x200000
  reg [31:0] gb_phase_inc = DEFAULT_GB_PHASE_INC;
  reg gb_stub_wr = 0;
  reg [14:0] gb_stub_wr_addr = 0;
`ifdef M4_HAS_HOME
  reg [14:0] gb_rom_dbg_addr = 0;
  wire [7:0] gb_rom_dbg_data;
  wire [47:0] gb_snd_touched;
`endif
  reg [7:0] gb_stub_wr_data = 0;

  // Загрузка сэмпл-ROM и мост чтения SegaPCM -> внешняя память.
  // Один арбитр: чтение ROM приоритетнее (загрузка идёт только при
  // остановленном чипе).
  reg [22:0] up_addr = 0;
  reg [15:0] up_data = 0;
  reg up_pending = 0;

`ifdef M4_HAS_ARCADE
  wire [18:0] pcm_rom_addr_w;
  wire pcm_rom_cs;
  reg rom_cs_prev = 0;
  reg rom_pending = 0;
  reg rom_wait_data = 0;
  reg [17:0] rom_word = 0;
  reg rom_byte = 0;
  reg [7:0] pcm_rom_data_r = 0;
`endif

  // DMC DMA: чтение DPCM-байтов из окна NES-RAM по запросу APU
  reg dmc_pending = 0;
  reg dmc_wait_data = 0;
  reg dmc_cool = 0;
  reg dmc_lane = 0;

  // Байтовые записи NES-RAM от секвенсора (DPCM-страницы из FIFO)
  reg fsm_wr_pending = 0;
  reg fsm_wr_lane = 0;
  reg [21:0] fsm_wr_word = 0;
  reg [7:0] fsm_wr_byte_l = 0;

  // Обслуживание ROM-чтений GBS (SM83)
  reg gbs_pending = 0;
  reg gbs_wait_data = 0;
  reg gbs_lane = 0;

  // Обслуживание чтений NSF-ROM для 6502
  reg nsf_inflight = 0;
  reg nsf_lane = 0;
  reg nsf_done = 0;
  reg [7:0] nsf_dbyte = 0;

  // ROM-выборка OKIM6295 (jt6295 в домене clk): следим за rom_addr,
  // подкачиваем байт, rom_ok=1 когда выбранный адрес совпал
`ifdef M4_HAS_ARCADE
  reg okim_pending = 0;
  reg okim_wait = 0;
  reg okim_req_byte = 0;
  reg [17:0] okim_req_addr = 0;
  reg [17:0] okim_fetched_addr = 18'h3FFFF;
`endif

  // ROM-выборка K053260: один фетчер по кругу для 4 канальных адресов
`ifdef M4_HAS_ARCADE
  reg k060_pending = 0;
  reg k060_wait = 0;
  reg [1:0] k060_rr = 0;         // указатель round-robin
  reg [1:0] k060_req_ch = 0;
  reg k060_req_byte = 0;
  reg [20:0] k060_req_addr = 0;
  reg [20:0] k060_fa0 = 21'h1FFFFF, k060_fa1 = 21'h1FFFFF,
             k060_fa2 = 21'h1FFFFF, k060_fa3 = 21'h1FFFFF;
`endif
`ifdef M4_HAS_ARCADE
  wire [20:0] k060_cur = k060_rr == 0 ? roma_addr : k060_rr == 1 ? romb_addr
                       : k060_rr == 2 ? romc_addr : romd_addr;
  wire [20:0] k060_cur_fa = k060_rr == 0 ? k060_fa0 : k060_rr == 1 ? k060_fa1
                          : k060_rr == 2 ? k060_fa2 : k060_fa3;
`endif

  always @(posedge clk) begin
    ack <= 0;
    // Транзакция шины обслуживается РОВНО ОДИН РАЗ.
    //
    // Условие было «stb && cyc && !ack», но ack — импульс: он сам себя
    // сбрасывает строкой выше, поэтому при удержании шины блок исполнялся
    // через такт, снова и снова. Регистрам без побочных действий это не
    // мешало, а VU (чтение 0x1A очищает пики) отдавал уже очищенный ноль:
    // на устройстве мастер держит шину дольше такта, и полоска стояла
    // мёртвой, притом что таймер по 0x18 шёл. Наш wb() в стенде снимал stb
    // сразу после ack и потому этого не видел — см. --vu-selftest.
    if (!(stb && cyc)) wb_served <= 0;
    soft_reset_req <= 0;
    stub_wr <= 0;
    gb_stub_wr <= 0;
    mem_rd <= 0;
    mem_wr <= 0;

`ifdef M4_HAS_ARCADE
    // Запросы чтения ROM от SegaPCM: защёлкиваем адрес по фронту cs
    rom_cs_prev <= pcm_rom_cs;
    if (pcm_rom_cs && !rom_cs_prev) begin
      rom_pending <= 1;
      rom_word <= pcm_rom_addr_w[18:1];
      rom_byte <= pcm_rom_addr_w[0];
    end
    if (mem_rdata_valid && rom_wait_data) begin
      pcm_rom_data_r <= rom_byte ? mem_rdata[15:8] : mem_rdata[7:0];
      rom_wait_data <= 0;
    end
`endif

    // DMC DMA: запрос APU -> чтение окна NES-RAM -> ack.
    //
    // Подтверждение держим, пока чип его не увидит. APU шагает по
    // cen_nes (~1.79 МГц), а этот домен идёт на 57 МГц: одноцикловый
    // импульс попадал на рабочий такт примерно раз из тридцати двух, и
    // канал получал ровно один байт за трек — первый, по случайному
    // совпадению. Дальше apu_dma_req оставался поднят, dmc_cool не
    // снимался, и выборки прекращались навсегда. Проверялось счётчиком
    // 5'h19: было «выборок из памяти 1» на 241 байт сэмпла.
    // Подтверждение должно длиться ровно один «put»-такт чипа. Внутри
    // DmcChan приём байта стоит под if (aclk1_d): пока ack поднят, на
    // КАЖДОМ таком такте заново защёлкивается байт, растёт адрес и тает
    // счётчик остатка. Держали до снятия запроса — сэмпл пролетал по
    // кругу, и вместо ударных выходил гул: на тесте со скоростью 4 у
    // эталона 70% энергии в 640-1250 Гц, у нас 77% в 40-80.
    //
    // Одного такта clk мало (чип идёт на своём разрешении и замечал
    // импульс раз из тридцати двух — была одна выборка за трек), одного
    // шага cen_nes тоже (56 из 241): нужен именно put-такт, то есть
    // cen_nes при nes_odd = 0, как собирает aclk1_d сам APU.
    if (apu_dma_ack && cen_nes && !nes_odd) apu_dma_ack <= 0;
    if (apu_dma_req && !dmc_pending && !dmc_wait_data && !dmc_cool) begin
      dmc_pending <= 1;
    end
    if (mem_rdata_valid && dmc_wait_data) begin
      apu_dma_data <= dmc_lane ? mem_rdata[15:8] : mem_rdata[7:0];
      apu_dma_ack <= 1;
      dmc_wait_data <= 0;
      dmc_cool <= 1;
    end
    if (dmc_cool && !apu_dma_req && !apu_dma_ack) dmc_cool <= 0;

    // Ответ CPU по чтению NSF-ROM
    nsf_done <= 0;
    if (mem_rdata_valid && nsf_inflight) begin
      nsf_dbyte <= nsf_lane ? mem_rdata[15:8] : mem_rdata[7:0];
      nsf_done <= 1;
      nsf_inflight <= 0;
      nsf_fetch <= nsf_fetch + 1'b1;
    end

`ifdef M4_HAS_HOME
    // ROM-чтения GBS: запрос по смене toggle (адрес квазистатичен)
    if (gbs_req_sync[2] != gbs_req_sync[1] && !gbs_pending && !gbs_wait_data) begin
      gbs_pending <= 1;
    end
    if (mem_rdata_valid && gbs_wait_data) begin
      gbs_rom_data <= gbs_lane ? mem_rdata[15:8] : mem_rdata[7:0];
      gbs_wait_data <= 0;
      gbs_fetch <= gbs_fetch + 1'b1;
    end
`endif

`ifdef M4_HAS_ARCADE
    // ROM-выборка OKIM6295: запрос при рассинхроне адреса
    if (okim_rom_addr != okim_fetched_addr && !okim_pending && !okim_wait) begin
      okim_pending <= 1;
      okim_rom_ok <= 0;
    end
    if (mem_rdata_valid && okim_wait) begin
      okim_rom_data_r <= okim_req_byte ? mem_rdata[15:8] : mem_rdata[7:0];
      okim_fetched_addr <= okim_req_addr;
      okim_wait <= 0;
      okim_rom_ok <= 1;
    end
`endif

`ifdef M4_HAS_ARCADE
    // ROM-выборка K053260: круговой опрос 4 каналов, устаревший -> фетч
    if (!k060_pending && !k060_wait) begin
      if (k060_cur != k060_cur_fa) begin
        k060_pending <= 1;
        k060_req_ch <= k060_rr;
        k060_req_addr <= k060_cur;
      end else begin
        k060_rr <= k060_rr + 1'b1;
      end
    end
    if (mem_rdata_valid && k060_wait) begin
      case (k060_req_ch)
        2'd0: begin roma_data_r <= k060_req_byte ? mem_rdata[15:8] : mem_rdata[7:0]; k060_fa0 <= k060_req_addr; end
        2'd1: begin romb_data_r <= k060_req_byte ? mem_rdata[15:8] : mem_rdata[7:0]; k060_fa1 <= k060_req_addr; end
        2'd2: begin romc_data_r <= k060_req_byte ? mem_rdata[15:8] : mem_rdata[7:0]; k060_fa2 <= k060_req_addr; end
        2'd3: begin romd_data_r <= k060_req_byte ? mem_rdata[15:8] : mem_rdata[7:0]; k060_fa3 <= k060_req_addr; end
      endcase
      k060_wait <= 0;
      k060_rr <= k060_rr + 1'b1;
    end
`endif

    // Байтовая запись: от секвенсора (DPCM в окно NES) либо от CPU
    // в sid-режиме (RAM в регионе NSF/SID)
    if (fsm_wr_req && !fsm_wr_pending) begin
      fsm_wr_pending <= 1;
      fsm_wr_word <= NES_BASE_WORD + {8'b0, fsm_wr_addr[14:1]};
      fsm_wr_lane <= fsm_wr_addr[0];
      fsm_wr_byte_l <= fsm_wr_data;
    end
`ifdef M4_HAS_HOME
    else if (sid_wr_req && !fsm_wr_pending) begin
      fsm_wr_pending <= 1;
      fsm_wr_word <= NSF_BASE_WORD + {7'b0, sid_wr_addr[15:1]};
      fsm_wr_lane <= sid_wr_addr[0];
      fsm_wr_byte_l <= sid_wr_data;
    end
`endif

    // отладочное чтение PSRAM: тоггл-запрос от WB-блока
    dbg_req_t_d <= dbg_req_t;
    if (dbg_req_t_d != dbg_req_t) begin
      dbg_rd_pend <= 1;
      dbg_rd_valid <= 0;
    end
    if (mem_rdata_valid && dbg_wait) begin
      dbg_rd_data <= dbg_lane ? mem_rdata[15:8] : mem_rdata[7:0];
      dbg_wait <= 0;
      dbg_rd_valid <= 1;
    end

    // Считаем именно выборки, а не такты: подтверждение теперь держится
    // до снятия запроса, и счёт по уровню давал бы девятикратный перебор
    apu_dma_ack_d <= apu_dma_ack;
    if (apu_dma_ack && !apu_dma_ack_d) dmc_cnt <= dmc_cnt + 1'b1;
    if (soft_reset_req) begin
      dmc_cnt <= 0;
      nsf_fetch <= 0;
      gbs_fetch <= 0;
    end
`ifdef M4_HAS_ARCADE
    // ADPCM-поток (MSM6258): команды секвенсора и запросы по drq
    pf_wr <= 0;
    if (pf_wr) pf_cnt <= pf_cnt + 1'b1;
    if (soft_reset_req) begin
      stream_active <= 0;
      pf_pending <= 0;
      pf_wait_data <= 0;
      pf_cnt <= 0;
    end
    if (str_set_addr) stream_addr <= str_payload[22:0];
    if (str_start) begin
      stream_len <= str_payload;
      stream_active <= str_payload != 0;
    end
    if (str_stop) stream_active <= 0;

    if (stream_active && adpcm_drq && !pf_pending && !pf_wait_data
        && !adpcm_wr_pending && !adpcm_fsm_wr && !pf_wr) begin
      pf_pending <= 1;
    end
    if (mem_rdata_valid && pf_wait_data) begin
      pf_byte <= pf_lane ? mem_rdata[15:8] : mem_rdata[7:0];
      pf_wr <= 1;
      pf_wait_data <= 0;
      stream_addr <= stream_addr + 23'd1;
      stream_len <= stream_len - 24'd1;
      if (stream_len == 24'd1) stream_active <= 0;
    end
`endif

    // Арбитр внешней памяти:
    // ROM SegaPCM > DMC > CPU NSF > ADPCM-поток > записи секвенсора > загрузка
    if (!mem_busy && !mem_rd && !mem_wr
        && !dmc_wait_data && !nsf_inflight && !gbs_wait_data && !dbg_wait
`ifdef M4_HAS_ARCADE
        && !rom_wait_data && !pf_wait_data && !okim_wait && !k060_wait
`endif
        ) begin
`ifdef M4_HAS_ARCADE
      if (rom_pending) begin
        mem_rd <= 1;
        mem_addr <= {4'b0, rom_word};
        rom_pending <= 0;
        rom_wait_data <= 1;
      end else if (okim_pending) begin
        mem_rd <= 1;
        mem_addr <= OKIM_BASE_WORD + {5'b0, okim_rom_addr[17:1]};
        okim_req_byte <= okim_rom_addr[0];
        okim_req_addr <= okim_rom_addr;
        okim_pending <= 0;
        okim_wait <= 1;
      end else if (k060_pending) begin
        mem_rd <= 1;
        mem_addr <= K060_BASE_WORD + {2'b0, k060_req_addr[20:1]};
        k060_req_byte <= k060_req_addr[0];
        k060_pending <= 0;
        k060_wait <= 1;
      end else if (dmc_pending) begin
`else
      if (dmc_pending) begin
`endif
        mem_rd <= 1;
        // в NSF-режиме DMC читает через банки, в VGM — плоское окно
        mem_addr <= nsf_mode
            ? NSF_BASE_WORD + {3'b0, nsf_banks[apu_dma_addr[14:12]], apu_dma_addr[11:1]}
            : NES_BASE_WORD + {8'b0, apu_dma_addr[14:1]};
        dmc_lane <= apu_dma_addr[0];
        dmc_pending <= 0;
        dmc_wait_data <= 1;
`ifdef M4_HAS_HOME
      end else if (gbs_pending) begin
        mem_rd <= 1;
        mem_addr <= NSF_BASE_WORD + {3'b0, gbs_rom_addr[19:1]};
        gbs_lane <= gbs_rom_addr[0];
        gbs_pending <= 0;
        gbs_wait_data <= 1;
`endif
      end else if (fsm_wr_pending) begin
        mem_wr <= 1;
        mem_addr <= fsm_wr_word;
        mem_wdata <= {fsm_wr_byte_l, fsm_wr_byte_l};
        mem_wbe <= fsm_wr_lane ? 2'b10 : 2'b01;
        fsm_wr_pending <= 0;
      end else if (nsf_req && !nsf_done) begin
        mem_rd <= 1;
        mem_addr <= NSF_BASE_WORD + {3'b0, nsf_rom_byte_addr[19:1]};
        nsf_lane <= nsf_rom_byte_addr[0];
        nsf_inflight <= 1;
`ifdef M4_HAS_ARCADE
      end else if (pf_pending) begin
        mem_rd <= 1;
        mem_addr <= stream_addr[22:1];
        pf_lane <= stream_addr[0];
        pf_pending <= 0;
        pf_wait_data <= 1;
`endif
      end else if (dbg_rd_pend) begin
        mem_rd <= 1;
        mem_addr <= dbg_rd_addr[22:1];
        dbg_lane <= dbg_rd_addr[0];
        dbg_rd_pend <= 0;
        dbg_wait <= 1;
      end else if (up_pending) begin
        mem_wr <= 1;
        mem_addr <= up_addr[22:1];
        mem_wdata <= up_data;
        mem_wbe <= 2'b11;
        up_addr <= up_addr + 23'd2;
        up_pending <= 0;
      end
    end

    if (reset) begin
      wr_ptr <= 0;
      phase_inc <= DEFAULT_PHASE_INC;
      ay_phase_inc <= DEFAULT_AY_PHASE_INC;
      pcm_phase_inc <= DEFAULT_PCM_PHASE_INC;
      up_pending <= 0;
`ifdef M4_HAS_ARCADE
      rom_pending <= 0;
      rom_wait_data <= 0;
      stream_active <= 0;
      pf_pending <= 0;
      pf_wait_data <= 0;
      pf_wr <= 0;
`endif
      dmc_pending <= 0;
      dmc_wait_data <= 0;
      dmc_cool <= 0;
      fsm_wr_pending <= 0;
      nsf_inflight <= 0;
      nsf_done <= 0;
      gbs_pending <= 0;
      gbs_wait_data <= 0;
    end else if (stb && cyc && !wb_served) begin
      if (we) begin
        ack <= 1;
        wb_served <= 1;
        case (addr[5:0])
          4'h0: begin
            if (!fifo_full) begin
              fifo_mem[wr_ptr[FIFO_AW-1:0]] <= data_write;
              wr_ptr <= wr_ptr + 1'b1;
            end
          end
          5'h2: begin
            soft_reset_req <= data_write[0];
            nsf_mode <= data_write[1];
            cpu_run <= data_write[2];
            gbs_mode <= data_write[3];
            sid_mode <= data_write[4];
            pause_r <= data_write[5];
            ff_r <= data_write[6];
            vrc6_en <= data_write[7];
            gb_apu_mode <= data_write[8];
            // софт-сброс обязан чистить и запись: rd_ptr обнуляется в
            // секвенсоре, и без обнуления wr_ptr FIFO «воскресает» со
            // старым содержимым кольца (переигрывание прошлого трека)
            if (data_write[0]) wr_ptr <= 0;
          end
          5'h12: sid_phase_inc <= data_write;
          5'h13: sid_v8580 <= data_write[0];
          5'h14: opl_phase_inc <= data_write;
          5'h15: {sn_gain, fm_gain} <= data_write[15:0];
          5'h16: fm_phase_inc <= data_write;
          5'h17: sn_phase_inc <= data_write;
`ifdef M4_HAS_HOME
          6'h21: scc_phase_inc <= data_write;
          6'h22: scc_gain <= data_write[7:0];
`ifdef M4_HAS_HOME
          6'h29: gb_rom_dbg_addr <= data_write[14:0];
`endif
          6'h27: huc_phase_inc <= data_write;
          6'h28: huc_gain <= data_write[7:0];
          6'h2C: md_lpf_mode <= data_write[1:0];
          6'h2D: nes_flt_mode <= data_write[1:0];
          6'h2E: {sn_sr15, sn_fb_mask} <= data_write[16:0];
          6'h2F: sn_stereo_en <= data_write[0];
`endif
`ifdef M4_HAS_ARCADE
          6'h23: okim_phase_inc <= data_write;
          6'h24: {okim_ss, okim_gain} <= data_write[8:0];
          6'h25: k060_phase_inc <= data_write;
          6'h26: k060_gain <= data_write[7:0];
`endif
          5'h10: gb_phase_inc <= data_write;
          5'h11: begin
            gb_stub_wr_addr <= data_write[22:8];
            gb_stub_wr_data <= data_write[7:0];
            gb_stub_wr <= 1;
          end
          4'h3: phase_inc <= data_write;
          4'h4: ay_phase_inc <= data_write;
          4'h5: pcm_phase_inc <= data_write;
          4'h6: mix_gains <= data_write;
          4'h7: adpcm_phase_inc <= data_write;
          4'hA: adpcm_clkdiv <= data_write[1:0];
          4'hB: nes_phase_inc <= data_write;
          4'hC: {opl_gain, sid_gain, gb_gain, apu_gain} <= data_write;
          4'hD: begin
            stub_wr_addr <= data_write[15:8];
            stub_wr_data <= data_write[7:0];
            stub_wr <= 1;
          end
          4'hE: vector_regs[data_write[10:8]] <= data_write[7:0];
          4'hF: play_phase_inc <= data_write;
          5'h1F: begin
            dbg_rd_addr <= data_write[22:0];
            dbg_req_t <= ~dbg_req_t;
          end
          4'h8: up_addr <= data_write[22:0];
          4'h9: begin
            // стойло, пока предыдущее слово не ушло в память
            if (!up_pending) begin
              up_data <= data_write[15:0];
              up_pending <= 1;
            end else begin
              // стойло: ack не отдаём и транзакцию не считаем
              // обслуженной, чтобы мастер получил ответ следующим тактом
              ack <= 0;
              wb_served <= 0;
            end
          end
          default: ;
        endcase
      end else begin
        ack <= 1;
        wb_served <= 1;
        case (addr[5:0])
          4'h1: data_read <= {fifo_full, fifo_empty, seq_busy, 16'h0, fifo_used};
          4'h2: data_read <= {dbg_apu_wr, dbg_apu_ce, cpu_ab};
          4'h0: data_read <= {11'b0, dbg_last_a, 8'b0, dbg_last_d};
          4'h3: data_read <= phase_inc;
          4'h4: data_read <= ay_phase_inc;
          4'h5: data_read <= pcm_phase_inc;
          4'h6: data_read <= mix_gains;
          4'h7: data_read <= adpcm_phase_inc;
          4'hA: data_read <= {30'h0, adpcm_clkdiv};
          4'hB: data_read <= nes_phase_inc;
          4'hC: data_read <= {opl_gain, sid_gain, gb_gain, apu_gain};
          5'h14: data_read <= opl_phase_inc;
          5'h15: data_read <= {16'h0, sn_gain, fm_gain};
          5'h16: data_read <= fm_phase_inc;
          5'h17: data_read <= sn_phase_inc;
          5'h12: data_read <= sid_phase_inc;
          5'h10: data_read <= gb_phase_inc;
`ifdef M4_HAS_HOME
          6'h29: data_read <= {24'b0, gb_rom_dbg_data};
          6'h2A: data_read <= gb_snd_touched[31:0];
          6'h2B: data_read <= {16'b0, gb_snd_touched[47:32]};
`endif
          5'h18: data_read <= tick_count;
          5'h19: data_read <= {dmc_cnt, pf_cnt};
          5'h1B: data_read <= {p_acks, snd_wr};
          5'h1C: data_read <= {gb_beat, opl_beat};
          5'h1D: data_read <= {gbs_fetch, nsf_fetch};
          5'h1E: data_read <= {gbs_ticks, gbs_sndwr};
          6'h20: data_read <= {8'b0, slot_upd_info};
          5'h1F: data_read <= {23'b0, dbg_rd_valid, dbg_rd_data};
          5'h1A: begin
            data_read <= {vu_r, vu_l};
            vu_take <= ~vu_take; // очистить пики после чтения
          end
          default: data_read <= 32'h0;
        endcase
      end
    end
  end

  // --------------------------------------------------------------------
  // Сброс чипа: держим несколько сотен тактов, чтобы захватить cen
  // chip_reset — регистр с maxfan: фан-аут ~3К на все чипы душил
  // трассировку; синтез дублирует регистр (GLOBAL_SIGNAL крашит фиттер 21.1)
  reg [9:0] chip_reset_cnt = 10'h3FF;
  reg chip_reset = 1 /* synthesis maxfan = 300 */;

  always @(posedge clk) begin
    if (reset || soft_reset_req) begin
      chip_reset_cnt <= 10'h3FF;
      chip_reset <= 1;
    end else if (chip_reset_cnt != 0) begin
      chip_reset_cnt <= chip_reset_cnt - 1'b1;
      chip_reset <= 1;
    end else begin
      chip_reset <= 0;
    end
  end

  // --------------------------------------------------------------------
  // Clock-enable чипа (дробный делитель) и тики 1/44100 с
  reg [31:0] cen_phase = 0;
`ifdef M4_HAS_ARCADE
  reg cen = 0;
  reg cen_p1_toggle = 0;

  always @(posedge clk) begin
    cen <= 0;
    if (!pause_r) {cen, cen_phase} <= {1'b0, cen_phase} + {1'b0, phase_inc};
    if (cen) cen_p1_toggle <= ~cen_p1_toggle;
  end

  wire cen_p1 = cen && cen_p1_toggle;
`endif

  reg [31:0] ay_cen_phase = 0;
  reg cen_ay = 0;

  always @(posedge clk) begin
    cen_ay <= 0;
    if (!pause_r) {cen_ay, ay_cen_phase} <= {1'b0, ay_cen_phase} + {1'b0, ay_phase_inc};
  end

  reg [31:0] fm_cen_phase = 0;
  reg cen_fm = 0;
  reg [31:0] sn_cen_phase = 0;
  reg cen_sn = 0;

  always @(posedge clk) begin
    cen_fm <= 0;
    cen_sn <= 0;
    if (!pause_r) begin
      {cen_fm, fm_cen_phase} <= {1'b0, fm_cen_phase} + {1'b0, fm_phase_inc};
      {cen_sn, sn_cen_phase} <= {1'b0, sn_cen_phase} + {1'b0, sn_phase_inc};
    end
  end

  reg [31:0] pcm_cen_phase = 0;
  reg cen_pcm = 0;

  always @(posedge clk) begin
    cen_pcm <= 0;
    if (!pause_r) {cen_pcm, pcm_cen_phase} <= {1'b0, pcm_cen_phase} + {1'b0, pcm_phase_inc};
  end

  reg [31:0] sid_cen_phase = 0;
`ifdef M4_HAS_HOME
  reg cen_sid = 0;

  always @(posedge clk) begin
    cen_sid <= 0;
    if (!pause_r) {cen_sid, sid_cen_phase} <= {1'b0, sid_cen_phase} + {1'b0, sid_phase_inc};
  end
`else
  wire cen_sid = 0;
`endif

  reg [31:0] adpcm_cen_phase = 0;
  reg cen_adpcm = 0;

  always @(posedge clk) begin
    cen_adpcm <= 0;
    if (!pause_r) {cen_adpcm, adpcm_cen_phase} <= {1'b0, adpcm_cen_phase} + {1'b0, adpcm_phase_inc};
  end

  // Клок NES: MSB фазового аккумулятора даёт PHI2 (высок вторую половину
  // CPU-цикла), перенос — clock-enable; чёт/нечет цикла — тоггл.
  reg [31:0] nes_cen_phase = 0;
  reg cen_nes = 0;
  reg nes_odd = 0;
  reg nes_phi2_d = 0;

  wire nes_phi2 = nes_cen_phase[31];

  always @(posedge clk) begin
    cen_nes <= 0;
    if (!pause_r) {cen_nes, nes_cen_phase} <= {1'b0, nes_cen_phase} + {1'b0, nes_phase_inc};
    if (cen_nes) nes_odd <= ~nes_odd;
    nes_phi2_d <= nes_phi2;
  end

`ifdef M4_HAS_HOME
  // Клок Konami SCC (~1.79 МГц)
  reg [31:0] scc_cen_phase = 0;
  reg cen_scc = 0;
  always @(posedge clk) begin
    cen_scc <= 0;
    if (!pause_r) {cen_scc, scc_cen_phase} <= {1'b0, scc_cen_phase} + {1'b0, scc_phase_inc};
  end

  // Клок HuC6280 PSG (3.579545 МГц)
  reg [31:0] huc_cen_phase = 0;
  reg cen_huc = 0;
  always @(posedge clk) begin
    cen_huc <= 0;
    if (!pause_r) {cen_huc, huc_cen_phase} <= {1'b0, huc_cen_phase} + {1'b0, huc_phase_inc};
  end
`endif

`ifdef M4_HAS_ARCADE
  // Клок OKIM6295 (~1 МГц)
  reg [31:0] okim_cen_phase = 0;
  reg cen_okim = 0;
  always @(posedge clk) begin
    cen_okim <= 0;
    if (!pause_r) {cen_okim, okim_cen_phase} <= {1'b0, okim_cen_phase} + {1'b0, okim_phase_inc};
  end

  // Клок K053260 (~3.58 МГц)
  reg [31:0] k060_cen_phase = 0;
  reg cen_k060 = 0;
  always @(posedge clk) begin
    cen_k060 <= 0;
    if (!pause_r) {cen_k060, k060_cen_phase} <= {1'b0, k060_cen_phase} + {1'b0, k060_phase_inc};
  end
`endif

  localparam [31:0] TICK_RATE = 44_100;
  reg [31:0] tick_acc = 0;
  reg tick = 0;
  wire [31:0] tick_step = ff_r ? TICK_RATE << 3 : TICK_RATE;

  always @(posedge clk) begin
    tick <= 0;
    if (chip_reset) begin
      tick_acc <= 0;
    end else if (pause_r) begin
      // пауза: тики секвенсора стоят
    end else if (tick_acc + tick_step >= CLK_HZ) begin
      tick_acc <= tick_acc + tick_step - CLK_HZ;
      tick <= 1;
    end else begin
      tick_acc <= tick_acc + tick_step;
    end
  end

  // --------------------------------------------------------------------
  // YM2151 (аркадный вариант; на нём же X68000)
`ifdef M4_HAS_ARCADE
  reg ym_cs_n = 1;
  reg ym_wr_n = 1;
  reg ym_a0 = 0;
  reg [7:0] ym_din = 0;
  wire [7:0] ym_dout;
  wire ym_sample;
  wire signed [15:0] ym_xleft;
  wire signed [15:0] ym_xright;

  jt51 ym2151 (
      .rst(chip_reset),
      .clk(clk),
      .cen(cen),
      .cen_p1(cen_p1),
      .cs_n(ym_cs_n),
      .wr_n(ym_wr_n),
      .a0(ym_a0),
      .din(ym_din),
      .dout(ym_dout),
      .ct1(),
      .ct2(),
      .irq_n(),
      .sample(ym_sample),
      .left(),
      .right(),
      .xleft(ym_xleft),
      .xright(ym_xright)
  );

  wire ym_busy = ym_dout[7];

  reg signed [15:0] ym_l = 0;
  reg signed [15:0] ym_r = 0;

  reg ym_sample_prev = 0;
  always @(posedge clk) begin
    ym_sample_prev <= ym_sample;
    if (ym_sample && !ym_sample_prev) begin
      ym_l <= ym_xleft;
      ym_r <= ym_xright;
    end
  end
`endif

  // --------------------------------------------------------------------
  // AY-3-8910 / YM2149 (jt49)
  reg ay_cs_n = 1;
  reg ay_wr_n = 1;
  reg [3:0] ay_addr = 0;
  reg [7:0] ay_din = 0;
  wire [9:0] ay_sound;

  // NSF Sunsoft 5B: 6502 пишет $C000 (выбор регистра) / $E000 (данные)
  reg [3:0] nsf_5b_reg = 0;
  reg ay_cs_cpu_n = 1;
  reg ay_wr_cpu_n = 1;
  reg [7:0] ay_din_cpu = 0;

  wire ay_cs_mux_n = ay_cs_n & ay_cs_cpu_n;
  wire ay_wr_mux_n = ay_wr_n & ay_wr_cpu_n;
  wire [3:0] ay_addr_mux = !ay_cs_cpu_n ? nsf_5b_reg : ay_addr;
  wire [7:0] ay_din_mux = !ay_cs_cpu_n ? ay_din_cpu : ay_din;

  jt49 ay (
      .rst_n(~chip_reset),
      .clk(clk),
      .clk_en(cen_ay),
      .addr(ay_addr_mux),
      .cs_n(ay_cs_mux_n),
      .wr_n(ay_wr_mux_n),
      .din(ay_din_mux),
      .sel(1'b1),
      .dout(),
      .sound(ay_sound),
      .A(),
      .B(),
      .C(),
      .sample(),
      .IOA_in(8'h0),
      .IOA_out(),
      .IOA_oe(),
      .IOB_in(8'h0),
      .IOB_out(),
      .IOB_oe()
  );

  // --------------------------------------------------------------------
  // SegaPCM (315-5218, jtoutrun_pcm): 16 каналов 8-бит сэмплов из
  // внешней памяти, стерео. cen = 2 x клок чипа из VGM.
`ifdef M4_HAS_ARCADE
  reg pcm_cs = 0;
  reg [7:0] pcm_addr = 0;
  reg [7:0] pcm_din = 0;
  wire signed [15:0] pcm_l;
  wire signed [15:0] pcm_r;
  wire pcm_sample;

  jtoutrun_pcm sega_pcm (
      .rst(chip_reset),
      .clk(clk),
      .cen(cen_pcm),

      .debug_bus(8'h0),
      .st_dout(),

      .cpu_addr(pcm_addr),
      .cpu_dout(pcm_din),
      .cpu_din(),
      .cpu_rnw(~pcm_cs),
      .cpu_cs(pcm_cs),

      .rom_addr(pcm_rom_addr_w),
      .rom_data(pcm_rom_data_r),
      .rom_ok(1'b1),
      .rom_cs(pcm_rom_cs),

      .snd_left(pcm_l),
      .snd_right(pcm_r),
      .sample(pcm_sample)
  );
`endif

  // --------------------------------------------------------------------
  // MSM6258 (ADPCM X68000) + префетчер потока из памяти сэмплов.
  // Секвенсор пишет регистры чипа (опкод 0x4) и управляет потоком
  // (0x5/0x6/0x7); префетчер сам подаёт байты по drq чипа — аналог DMA.
`ifdef M4_HAS_ARCADE
  reg adpcm_fsm_wr = 0;
  reg [1:0] adpcm_fsm_addr = 0;
  reg [7:0] adpcm_fsm_din = 0;
  reg [1:0] adpcm_pan = 0;

  reg str_set_addr = 0;
  reg str_start = 0;
  reg str_stop = 0;
  reg [23:0] str_payload = 0;

  reg [22:0] stream_addr = 0;
  reg [23:0] stream_len = 0;
  reg stream_active = 0;
  reg pf_pending = 0;
  reg pf_wait_data = 0;
  reg pf_lane = 0;
  reg pf_wr = 0;
  reg [7:0] pf_byte = 0;

  wire adpcm_drq;
  wire adpcm_wr_pending;
  wire signed [11:0] adpcm_sound;

  // строб в чип: либо от секвенсора (регистры), либо от префетчера (данные)
  wire adpcm_wr = adpcm_fsm_wr || pf_wr;
  wire adpcm_addr = adpcm_fsm_wr ? adpcm_fsm_addr[0] : 1'b1;
  wire [7:0] adpcm_din = adpcm_fsm_wr ? adpcm_fsm_din : pf_byte;

  msm6258 adpcm (
      .clk(clk),
      .rst(chip_reset),
      .cen(cen_adpcm),
      .clkdiv(adpcm_clkdiv),

      .wr(adpcm_wr),
      .addr(adpcm_addr),
      .din(adpcm_din),

      .drq(adpcm_drq),
      .wr_pending(adpcm_wr_pending),
      .playing(),
      .sound(adpcm_sound)
  );
`endif

  // --------------------------------------------------------------------
  // Konami SCC (K051649, IKASCC). Волновая таблица, без внешнего ROM.
  // Секвенсор гоняет шину MSX: разблокировка BR2=0x3F (ABHI=0x12), затем
  // регистры звука при ABHI=0x13. Строб CS/WR держится счётом cen_scc:
  // write_request в чипе ловит переход (CS|WR) 1->0 по своему клоку.
`ifdef M4_HAS_HOME
  reg scc_cs_n = 1;
  reg scc_wr_n = 1;
  reg [4:0] scc_abhi = 0;
  reg [7:0] scc_ablo = 0;
  reg [7:0] scc_db = 0;
`endif
`ifdef M4_HAS_HOME
  // HuC6280 PSG (PC Engine). Запись однотактовая, строб не нужен.
  reg        huc_wr = 0;
  reg  [3:0] huc_addr = 0;
  reg  [7:0] huc_din = 0;
  wire signed [15:0] huc_left, huc_right;
  huc6280_psg huc (
      .clk(clk),
      .cen(cen_huc),
      .rst(chip_reset),
      .wr(huc_wr),
      .addr(huc_addr),
      .din(huc_din),
      .snd_left(huc_left),
      .snd_right(huc_right)
  );

  wire signed [10:0] scc_sound;

  IKASCC #(
      .IMPL_TYPE(0),
      .RAM_BLOCK(1)
  ) scc (
      .i_EMUCLK(clk),
      .i_MCLK_PCEN_n(~cen_scc),
      .i_RST_n(~chip_reset),
      .i_CS_n(scc_cs_n),
      .i_RD_n(1'b1),
      .i_WR_n(scc_wr_n),
      .i_ABLO(scc_ablo),
      .i_ABHI(scc_abhi),
      .i_DB(scc_db),
      .o_DB(),
      .o_DB_OE(),
      .o_ROMCS_n(),
      .o_ROMADDR(),
      .o_SOUND(scc_sound),
      .o_TEST()
  );
`endif

  // --------------------------------------------------------------------
  // OKIM6295 (jt6295): ADPCM с сэмплами из PSRAM. АРКАДНЫЙ чип — вынесен из
  // битстрима под M4_SIM (см. историю площади, docs/chips.md): в железо идут
  // только домашние чипы, аркадные держатся в симуляции как задел.
`ifdef M4_HAS_ARCADE
  reg okim_wrn = 1;
  reg [7:0] okim_din = 0;
`endif
`ifdef M4_HAS_ARCADE
  wire [17:0] okim_rom_addr;
  reg [7:0] okim_rom_data_r = 0;
  reg okim_rom_ok = 0;
  wire signed [13:0] okim_sound;

  jt6295 #(
      .INTERPOL(0)
  ) okim (
      .rst(chip_reset),
      .clk(clk),
      .cen(cen_okim),
      .ss(okim_ss),
      .wrn(okim_wrn),
      .din(okim_din),
      .dout(),
      .rom_addr(okim_rom_addr),
      .rom_data(okim_rom_data_r),
      .rom_ok(okim_rom_ok),
      .sound(okim_sound),
      .sample()
  );
`endif

  // --------------------------------------------------------------------
  // K053260 (jt053260): 4 канала PCM/ADPCM. НЕ помещался в битстрим вместе
  // с SCC+OKIM6295 (99% ALM, slack -3.75) — по решению пользователя дропнут
  // из Quartus-сборки, но остаётся под M4_SIM (симуляция + селфтест). Чтобы
  // вернуть на железе: определить M4_SIM-эквивалент и освободить ~1К ALM.
`ifdef M4_HAS_ARCADE
  reg k060_cs = 0;
  reg k060_wr_n = 1;
  reg [5:0] k060_addr = 0;
  reg [7:0] k060_din = 0;
`endif
`ifdef M4_HAS_ARCADE
  wire [20:0] roma_addr, romb_addr, romc_addr, romd_addr;
  reg [7:0] roma_data_r = 0, romb_data_r = 0, romc_data_r = 0, romd_data_r = 0;
  wire signed [15:0] k060_l, k060_r;

  // NUMCH=2: урезан до 2 каналов (см. историю площади). Вернуть 4 — сменить.
  jt053260 #(.NUMCH(2)) k060 (
      .rst(chip_reset),
      .clk(clk),
      .cen(cen_k060),
      .ma0(1'b0),
      .mrdnw(1'b1),
      .mcs(1'b0),
      .mdout(8'd0),
      .mdin(),
      .addr(k060_addr),
      .wr_n(k060_wr_n),
      .rd_n(1'b1),
      .cs(k060_cs),
      .din(k060_din),
      .dout(),
      .roma_addr(roma_addr), .roma_data(roma_data_r), .roma_cs(),
      .romb_addr(romb_addr), .romb_data(romb_data_r), .romb_cs(),
      .romc_addr(romc_addr), .romc_data(romc_data_r), .romc_cs(),
      .romd_addr(romd_addr), .romd_data(romd_data_r), .romd_cs(),
      .aux_l(16'd0), .aux_r(16'd0),
      .snd_l(k060_l), .snd_r(k060_r),
      .sample(), .tim2(),
      .ch_en(5'b11111)
  );
`endif

  // --------------------------------------------------------------------
  // NES APU (2A03, из NES_MiSTer). Регистры пишутся стробом CS через
  // фронт PHI2; DMC сам тянет DPCM-байты из окна NES-RAM в памяти
  // сэмплов (DMA-движок ниже, в блоке арбитра).
  reg apu_cs = 0;
  reg [4:0] apu_addr = 0;
  reg [7:0] apu_din = 0;
  wire [15:0] apu_sample;
  wire [7:0] apu_dout;
  wire apu_dma_req;
  wire [15:0] apu_dma_addr;
  reg apu_dma_ack = 0;
  reg apu_dma_ack_d = 0;
  reg [7:0] apu_dma_data = 0;

  // Мультиплексирование доступа: VGM-секвенсор (только записи) либо
  // 6502 в режиме NSF
  wire apu_cs_mux = apu_cs | apu_cs_cpu;
  wire [4:0] apu_addr_mux = apu_cs_cpu ? apu_addr_cpu : apu_addr;
  wire [7:0] apu_din_mux = apu_cs_cpu ? apu_din_cpu : apu_din;
  wire apu_rw_mux = apu_cs ? 1'b0 : apu_cs_cpu ? ~cpu_we_hold : 1'b1;

  APU #(
      .SSREG_INDEX_TOP (10'd16),
      .SSREG_INDEX_DMC1(10'd17),
      .SSREG_INDEX_DMC2(10'd18),
      .SSREG_INDEX_FCT (10'd19)
  ) nes_apu (
      .MMC5(1'b0),
      .clk(clk),
      .PHI2(nes_phi2),
      .ce(cen_nes),
      .reset(chip_reset),
      .cold_reset(chip_reset),
      .allow_us(1'b0),
      .PAL(1'b0),
      .ADDR(apu_addr_mux),
      .DIN(apu_din_mux),
      .RW(apu_rw_mux),
      .CS(apu_cs_mux),
      .audio_channels(5'b11111),
      .DmaData(apu_dma_data),
      .get_or_put(nes_odd),
      .DmaAck(apu_dma_ack),
      .DOUT(apu_dout),
      .Sample(apu_sample),
      .DmaReq(apu_dma_req),
      .DmaAddr(apu_dma_addr),
      .IRQ(),
      .get_ce(),
      .put_ce(),
      .SaveStateBus_Din(64'd0),
      .SaveStateBus_Adr(10'd0),
      .SaveStateBus_wren(1'b0),
      .SaveStateBus_rst(1'b0),
      .SaveStateBus_load(1'b0),
      .SaveStateBus_Dout()
  );

  // --------------------------------------------------------------------
  // NSF: 6502 (Arlet) + карта памяти NES-плеера. CPU тактуется через
  // RDY: продвигается только на cen_nes и только когда данные готовы.
  reg [7:0] nsf_ram[2048];
  reg [7:0] nsf_wram[8192];
  reg [7:0] nsf_stub[256];
  reg [7:0] nsf_banks[8];

  // Каждый массив — в своём блоке с выделенным q-регистром, иначе
  // Quartus не выводит BRAM и разворачивает память в LUT.
  reg [7:0] nsf_ram_q, nsf_wram_q, nsf_stub_q;

  always @(posedge clk) begin
    nsf_ram_q <= nsf_ram[cpu_ab[10:0]];
    if (cpu_step && cpu_we && cpu_ab[15:11] == 5'b00000)
      nsf_ram[cpu_ab[10:0]] <= cpu_do;
  end

  always @(posedge clk) begin
    nsf_wram_q <= nsf_wram[cpu_ab[12:0]];
    if (cpu_step && cpu_we && cpu_ab[15:13] == 3'b011)
      nsf_wram[cpu_ab[12:0]] <= cpu_do;
  end

  always @(posedge clk) begin
    nsf_stub_q <= nsf_stub[cpu_ab[7:0]];
    if (stub_wr) nsf_stub[stub_wr_addr] <= stub_wr_data;
  end

  // play-тик
  reg [31:0] play_phase = 0;
  reg play_tick_carry = 0;
  reg play_pending = 0;

  always @(posedge clk) begin
    play_tick_carry <= 0;
    if (!pause_r) {play_tick_carry, play_phase} <= {1'b0, play_phase}
        + {1'b0, ff_r ? play_phase_inc << 3 : play_phase_inc};
  end

  wire [15:0] cpu_ab;
  wire [7:0] cpu_do;
  wire cpu_we;
  reg [7:0] cpu_di = 0;
  reg cpu_di_ready = 0;

  wire cpu_reset = chip_reset || !(nsf_mode || sid_mode) || !cpu_run;
  wire cen_cpu = sid_mode ? cen_sid : cen_nes;
  wire cpu_step = cen_cpu && cpu_di_ready;

  cpu nsf_cpu (
      .clk(clk),
      .reset(cpu_reset),
      .AB(cpu_ab),
      .DI(cpu_di),
      .DO(cpu_do),
      .WE(cpu_we),
      .IRQ(1'b0),
      .NMI(1'b0),
      .RDY(cpu_step)
  );

`ifdef M4_SIM
  assign dbg_gbs_rom_addr = gbs_rom_addr;
  assign dbg_gbs_rom_toggle = gbs_rom_req_toggle;
  assign dbg_cpu_ab = cpu_ab;
  assign dbg_cpu_step = cpu_step;
  assign dbg_cpu_we = cpu_we;
  assign dbg_cpu_di = cpu_di;
  assign dbg_cpu_do = cpu_do;
  assign dbg_apu_cs = apu_cs_mux;
  assign dbg_apu_din = dbg_last_d;
  assign dbg_apu_addr = dbg_last_a;
  assign dbg_phi2 = nes_phi2;
  assign dbg_strobes = dbg_apu_ce;
  assign dbg_sid_cs = sid_cs;
  assign dbg_sid_we = sid_we;
  assign dbg_sid_addr = sid_addr;
  assign dbg_sid_din = sid_din;
  assign dbg_sid_audio = sid_audio;
  reg [7:0] cen_sid_cnt = 0;
  always @(posedge clk) if (cen_sid) cen_sid_cnt <= cen_sid_cnt + 1'b1;
  assign dbg_cen_sid_cnt = cen_sid_cnt;
`endif

  // Контракт Arlet: адрес выставлен в цикле A (между step-фронтами),
  // данные потребляются на фронте, ЗАВЕРШАЮЩЕМ следующий цикл. Поэтому
  // адрес/WE/DO защёлкиваются на step-фронте (пред-фронтовые значения
  // цикла A), декодируется защёлкнутое.
  reg [15:0] cpu_ab_l = 0;
  reg cpu_we_l = 0;
  reg [7:0] cpu_do_l = 0;

  // Чтение NSF-ROM: cpu-блок держит nsf_req, арбитр отвечает пульсом
  // nsf_done с байтом (nsf_dbyte). Трансляция банков — здесь.
  reg nsf_req = 0;
  wire [7:0] nsf_bank_sel = nsf_banks[cpu_ab_l[14:12]];
  wire [19:0] nsf_rom_byte_addr = sid_mode
      ? {4'b0, cpu_ab_l}
      : {nsf_bank_sel, cpu_ab_l[11:0]};

  // Шинный адаптер: после шага CPU декодируем новый адрес и готовим DI
  reg cpu_step_d = 0;
  reg apu_cs_cpu = 0;
  reg cpu_we_hold = 0;
`ifdef M4_HAS_HOME
  reg sid_wr_hold = 0;
  reg sid_wr_req = 0;
  reg [15:0] sid_wr_addr = 0;
  reg [7:0] sid_wr_data = 0;
`endif
  reg sid_read_pend = 0;
  reg sid_cs = 0;
  reg sid_we = 0;
  reg [4:0] sid_addr = 0;
  reg [7:0] sid_din = 0;
  reg [4:0] apu_addr_cpu = 0;
  reg [7:0] apu_din_cpu = 0;
  reg apu_read_pend = 0;
  reg [7:0] dbg_apu_wr = 0;
  reg [7:0] dbg_apu_ce = 0;
  reg [4:0] dbg_last_a = 0;
  reg [7:0] dbg_last_d = 0;

  always @(posedge clk) begin
    if (apu_cs_mux && !apu_rw_mux && nes_phi2 && !nes_phi2_d) begin
      dbg_apu_ce <= dbg_apu_ce + 1'b1;
      dbg_last_a <= apu_addr_mux;
      dbg_last_d <= apu_din_mux;
    end
  end

  always @(posedge clk) begin
    cpu_step_d <= cpu_step;

    if (play_tick_carry) play_pending <= 1;

    vrc6_wr <= 0;
    if (cpu_reset) begin
      cpu_di_ready <= 1;
      p_acks <= 0;
      snd_wr <= 0;
      apu_cs_cpu <= 0;
      apu_read_pend <= 0;
      play_pending <= 0;
      nsf_req <= 0;
      ay_cs_cpu_n <= 1;
      ay_wr_cpu_n <= 1;
`ifdef M4_HAS_HOME
      sid_wr_hold <= 0;
      sid_wr_req <= 0;
`endif
      sid_read_pend <= 0;
      sid_cs <= 0;
      nsf_banks[0] <= 8'd0;
      nsf_banks[1] <= 8'd1;
      nsf_banks[2] <= 8'd2;
      nsf_banks[3] <= 8'd3;
      nsf_banks[4] <= 8'd4;
      nsf_banks[5] <= 8'd5;
      nsf_banks[6] <= 8'd6;
      nsf_banks[7] <= 8'd7;
    end else begin
      // снятие строба APU после фронта PHI2 (запись/чтение защёлкнуты)
      if (apu_cs_cpu && nes_phi2 && !nes_phi2_d) apu_cs_cpu <= 0;
      if (!ay_cs_cpu_n) begin
        ay_cs_cpu_n <= 1;
        ay_wr_cpu_n <= 1;
      end

      if (apu_read_pend && !cpu_di_ready) begin
        cpu_di <= apu_dout;
        cpu_di_ready <= 1;
        apu_read_pend <= 0;
      end

      if (nsf_done) begin
        cpu_di <= nsf_dbyte;
        cpu_di_ready <= 1;
        nsf_req <= 0;
      end

      if (cpu_step) begin
        cpu_ab_l <= cpu_ab;
        cpu_we_l <= cpu_we;
        cpu_do_l <= cpu_do;
      end

`ifdef M4_HAS_HOME
      // отложенная запись RAM (sid): ждём свободный канал
      if (sid_wr_hold && !fsm_wr_pending && !fsm_wr_req && !sid_wr_req) begin
        sid_wr_req <= 1;
        sid_wr_hold <= 0;
        cpu_di_ready <= 1;
      end
      if (sid_wr_req) sid_wr_req <= 0;
`endif

      if (sid_read_pend && !cpu_di_ready) begin
        cpu_di <= sid_dout;
        cpu_di_ready <= 1;
        sid_read_pend <= 0;
      end
      sid_cs <= 0;

`ifdef M4_HAS_HOME
      if (cpu_step_d && sid_mode) begin
        // Карта памяти PSID: SID @$D400-$D7EF, тик @$D7F0, всё
        // остальное — RAM в PSRAM (включая векторы)
        cpu_di_ready <= 0;
        if (cpu_we_l) begin
          if (cpu_ab_l[15:10] == 6'b110101) begin
            cpu_di_ready <= 1;
            if (cpu_ab_l[9:0] == 10'h3F0) begin
              play_pending <= 0;
              p_acks <= p_acks + 1'b1;
            end else begin
              sid_cs <= 1;
              sid_we <= 1;
              snd_wr <= snd_wr + 1'b1;
              sid_addr <= cpu_ab_l[4:0];
              sid_din <= cpu_do_l;
            end
          end else begin
            sid_wr_hold <= 1;
            sid_wr_addr <= cpu_ab_l;
            sid_wr_data <= cpu_do_l;
          end
        end else begin
          if (cpu_ab_l[15:10] == 6'b110101) begin
            if (cpu_ab_l[9:0] == 10'h3F0) begin
              cpu_di <= {7'b0, play_pending};
              cpu_di_ready <= 1;
            end else begin
              sid_cs <= 1;
              sid_we <= 0;
              sid_addr <= cpu_ab_l[4:0];
              sid_read_pend <= 1;
            end
          end else if (cpu_ab_l[15:9] == 7'b0000000) begin
            // Нулевая страница и стек ($0000-$01FF) — из BRAM, а не из
            // PSRAM: в режиме SID туда уходило всё адресное пространство,
            // включая две страницы, по которым 6502 бьёт чаще всего.
            // Записи сюда и так шли в nsf_ram — правится только чтение.
            // Шире брать нельзя: стаб лежит по $0334 и живёт в PSRAM.
            cpu_di <= nsf_ram_q;
            cpu_di_ready <= 1;
          end else begin
            nsf_req <= 1;  // остальная память — из PSRAM
          end
        end
      end else
`endif
      if (cpu_step_d) begin
        // декодируем адрес цикла, завершившегося прошлым step-фронтом
        cpu_di_ready <= 0;

        if (cpu_we_l) begin
          // записи RAM/WRAM выполнены отдельными BRAM-блоками на
          // step-фронте; здесь только периферия
          cpu_di_ready <= 1;
          if (cpu_ab_l[15:5] == 11'b01000000000) begin
            apu_addr_cpu <= cpu_ab_l[4:0];
            apu_din_cpu <= cpu_do_l;
            apu_cs_cpu <= 1;
            cpu_we_hold <= 1;
            snd_wr <= snd_wr + 1'b1;
            dbg_apu_wr <= dbg_apu_wr + 1'b1;
          end else if (cpu_ab_l == 16'h5FF0) begin
            play_pending <= 0;
            p_acks <= p_acks + 1'b1;
          end
          else if (cpu_ab_l[15:3] == 13'b0101111111111) nsf_banks[cpu_ab_l[2:0]] <= cpu_do_l;
          else if (vrc6_en && cpu_ab_l[15:12] >= 4'h9 && cpu_ab_l[15:12] <= 4'hB) begin
            // VRC6: $9000-$9003 pulse1, $A000-$A002 pulse2, $B000-$B002 пила
            vrc6_wr <= 1;
            vrc6_blk <= cpu_ab_l[13:12] - 2'd1;
            vrc6_rsel <= cpu_ab_l[1:0];
            vrc6_din <= cpu_do_l;
          end
          else if (cpu_ab_l[15:12] == 4'hC) nsf_5b_reg <= cpu_do_l[3:0];
          else if (cpu_ab_l[15:12] == 4'hE) begin
            ay_din_cpu <= cpu_do_l;
            ay_cs_cpu_n <= 0;
            ay_wr_cpu_n <= 0;
          end
        end else begin
          // чтения
          if (cpu_ab_l[15:11] == 5'b00000) begin
            cpu_di <= nsf_ram_q;
            cpu_di_ready <= 1;
          end else if (cpu_ab_l[15:13] == 3'b011) begin
            cpu_di <= nsf_wram_q;
            cpu_di_ready <= 1;
          end else if (cpu_ab_l[15:8] == 8'h50) begin
            cpu_di <= nsf_stub_q;
            cpu_di_ready <= 1;
          end else if (cpu_ab_l == 16'h5FF0) begin
            cpu_di <= {7'b0, play_pending};
            cpu_di_ready <= 1;
          end else if (cpu_ab_l >= 16'hFFFA) begin
            cpu_di <= vector_regs[cpu_ab_l[2:0] - 3'd2];
            cpu_di_ready <= 1;
          end else if (cpu_ab_l[15:5] == 11'b01000000000) begin
            apu_addr_cpu <= cpu_ab_l[4:0];
            apu_cs_cpu <= 1;
            cpu_we_hold <= 0;
            apu_read_pend <= 1;
          end else if (cpu_ab_l[15]) begin
            nsf_req <= 1;  // PSRAM через банки (обслужит арбитр)
          end else begin
            cpu_di <= 8'hFF;  // неизвестная область
            cpu_di_ready <= 1;
          end
        end
      end
    end
  end

  // --------------------------------------------------------------------
  // SID (C64_MiSTer, один чип, mode: 0=6581/1=8580). Записи — строб
  // 1 такт (регистры защёлкиваются каждый clk), чтения ($D41B/$D41C) —
  // через sid_read_pend.
  wire [7:0] sid_dout;
`ifdef M4_HAS_HOME
  wire [17:0] sid_audio;

  sid_top #(
      .MULTI_FILTERS(0),
      .DUAL(0)
  ) sid (
      .reset(chip_reset),
      .clk(clk),
      .ce_1m(cen_sid),

      .cs(sid_cs),
      .we(sid_we),
      .addr(sid_addr),
      .data_in(sid_din),
      .data_out(sid_dout),

      .fc_offset_l(13'd0),
      .pot_x_l(8'hFF),
      .pot_y_l(8'hFF),
      .ext_in_l(18'd0),
      .audio_l(sid_audio),

      .fc_offset_r(13'd0),
      .pot_x_r(8'hFF),
      .pot_y_r(8'hFF),
      .ext_in_r(18'd0),
      .audio_r(),

      .filter_en(1'b1),
      .mode(sid_v8580),
      .cfg(2'b00),

      .ld_clk(1'b0),
      .ld_addr(12'd0),
      .ld_data(16'd0),
      .ld_wr(1'b0)
  );
`endif

  // --------------------------------------------------------------------
  // GBS: SM83 + GB APU (VerilogBoy, немодифицированный) в собственном
  // клоковом домене. gb_clk — регистровый клок ~4.19 МГц (тоггл на
  // переносе фазового аккумулятора 2x частоты).
`ifdef M4_HAS_HOME
  reg [31:0] gb_half_phase = 0;
  reg gb_half_carry = 0;
  reg gb_clk_r = 0;

  always @(posedge clk) begin
    gb_half_carry <= 0;
    if (!pause_r) {gb_half_carry, gb_half_phase} <= {1'b0, gb_half_phase} + {1'b0, gb_phase_inc};
    if (gb_half_carry) gb_clk_r <= ~gb_clk_r;
  end

  // Регистровый клок ОБЯЗАН ехать по глобальной сети: на общей
  // трассировке (фан-аут ~900) клоковый скью убивает домен на железе.
  // qsf GLOBAL_SIGNAL крашит фиттер 21.1 — явный примитив global работает.
  wire gb_clk;
`ifdef M4_SIM
  assign gb_clk = gb_clk_r;
`else
  global gb_clk_buf (.in(gb_clk_r), .out(gb_clk));
`endif

  reg play_toggle = 0;
  always @(posedge clk) begin
    if (play_tick_carry) play_toggle <= ~play_toggle;
  end

  wire gbs_reset = chip_reset || !gbs_mode || !cpu_run;
  // APU живёт и в режиме VGM, где рипа с кодом нет. Сброс SM83 и ROM
  // при этом остаётся прежним: процессор в таком файле не нужен.
  wire gb_apu_reset = chip_reset || !(gbs_mode ? cpu_run : gb_apu_mode);
  wire [22:0] gbs_rom_addr;
  wire gbs_rom_req_toggle;
  reg [7:0] gbs_rom_data = 0;
  wire [15:0] gb_left;
  wire [15:0] gb_right;

  // Прямая запись в APU Game Boy для VGM-файлов: адрес и данные держим
  // до смены тоггла, домен gb_clk ловит фронт.
  //
  // Пауза между записями обязана быть длиннее, чем нужно синхронизатору:
  // такт gb_clk это около 13.6 тактов clk, а фронт виден только на
  // третьем его такте. Стояло 31 такт — это всего 2.3 такта чипа, и при
  // плотном потоке записи терялись. Рипы Game Boy пишут щедро (в одном
  // файле по 3137 записей в каждый регистр), и на слух это выходило как
  // мимо идущие ударные почти во всех файлах. Теперь 127 тактов, около
  // девяти тактов чипа; пропускная способность остаётся под 450 тысяч
  // записей в секунду при потребности в единицы тысяч.
  reg gb_ext_toggle = 0;
  reg [7:0] gb_ext_addr = 0;
  reg [7:0] gb_ext_data = 0;
  reg [6:0] gb_ext_wait = 0;

  gbsbox gbs (
      .gb_clk(gb_clk),
      .rst(gbs_reset),
      .apu_rst(gb_apu_reset),

      .sys_clk(clk),
      .stub_wr(gb_stub_wr),
      .stub_wr_addr(gb_stub_wr_addr),
      .stub_wr_data(gb_stub_wr_data),

      .snd_touched(gb_snd_touched),
      .rom_dbg_addr(gb_rom_dbg_addr),
      .rom_dbg_data(gb_rom_dbg_data),
      .play_tick_toggle(play_toggle),

      .snd_ext_toggle(gb_ext_toggle),
      .snd_ext_addr(gb_ext_addr),
      .snd_ext_data(gb_ext_data),

      .rom_addr(gbs_rom_addr),
      .rom_req_toggle(gbs_rom_req_toggle),
      .rom_data(gbs_rom_data),

      .snd_left(gb_left),
      .snd_right(gb_right),

      .tick_seen_toggle(gbs_tick_seen_t),
      .sndwr_toggle(gbs_sndwr_t)
  );
`endif

  // диагностика GBS: тики, доставленные в gb-домен, и записи SM83 в
  // звуковые реги — тогглы синхронизируются и считаются в sys (WB 0x1E)
  wire gbs_tick_seen_t;
  wire gbs_sndwr_t;
  reg [2:0] gbs_tick_seen_sync = 0;
  reg [2:0] gbs_sndwr_sync = 0;
  reg [15:0] gbs_ticks = 0;
  reg [15:0] gbs_sndwr = 0;

  always @(posedge clk) begin
    gbs_tick_seen_sync <= {gbs_tick_seen_sync[1:0], gbs_tick_seen_t};
    gbs_sndwr_sync <= {gbs_sndwr_sync[1:0], gbs_sndwr_t};
    if (gbs_tick_seen_sync[2] != gbs_tick_seen_sync[1]) gbs_ticks <= gbs_ticks + 1'b1;
    if (gbs_sndwr_sync[2] != gbs_sndwr_sync[1]) gbs_sndwr <= gbs_sndwr + 1'b1;
    if (soft_reset_req) begin
      gbs_ticks <= 0;
      gbs_sndwr <= 0;
    end
  end

  // синхронизация ROM-запросов в clk-домен
`ifdef M4_HAS_HOME
  reg [2:0] gbs_req_sync = 0;
  always @(posedge clk) gbs_req_sync <= {gbs_req_sync[1:0], gbs_rom_req_toggle};
`endif

  // выход звука GB в clk-домен (квазистатичен на аудиочастотах)
`ifdef M4_HAS_HOME
  reg [15:0] gb_left_s1 = 0, gb_left_s = 0;
  reg [15:0] gb_right_s1 = 0, gb_right_s = 0;
  always @(posedge clk) begin
    gb_left_s1 <= gb_left;
    gb_left_s <= gb_left_s1;
    gb_right_s1 <= gb_right;
    gb_right_s <= gb_right_s1;
  end
`else
  wire [15:0] gb_left_s = 0, gb_right_s = 0;
`endif

  // --------------------------------------------------------------------
  // YM2612 (jt12) + SN76489 (jt89) — Sega Genesis / Master System
  reg fm_cs_n = 1;
  reg fm_wr_n = 1;
  reg [1:0] fm_addr = 0;
  reg [7:0] fm_din = 0;
  wire [7:0] fm_dout;
  wire signed [15:0] fm_snd_l;
  wire signed [15:0] fm_snd_r;
  wire fm_sample;

  jt12 fm2612 (
      .rst(chip_reset),
      .clk(clk),
      .cen(cen_fm),
      .din(fm_din),
      .addr(fm_addr),
      .cs_n(fm_cs_n),
      .wr_n(fm_wr_n),
      .dout(fm_dout),
      .irq_n(),
      .en_hifi_pcm(1'b0),  // без hifi-интерполятора DAC — экономия площади
      .snd_right(fm_snd_r),
      .snd_left(fm_snd_l),
      .snd_sample(fm_sample)
  );

  wire fm_busy = fm_dout[7];

  reg signed [15:0] fm_l = 0;
  reg signed [15:0] fm_r = 0;
  reg fm_sample_prev = 0;
  always @(posedge clk) begin
    fm_sample_prev <= fm_sample;
    if (fm_sample && !fm_sample_prev) begin
      fm_l <= fm_snd_l;
      fm_r <= fm_snd_r;
    end
  end

  reg sn_wr_n = 1;
  reg [7:0] sn_din = 0;
  wire signed [10:0] sn_sound;

  // Разновидность шума SN76489 из заголовка VGM: у Master System регистр
  // 16 бит и отводы 0 и 3, у SN76489AN (SG-1000, аркады, BBC) — 15 бит и
  // отводы 0 и 1. Сброс воспроизводит вариант Master System, как было.
  reg [15:0] sn_fb_mask = 16'h0009;
  reg sn_sr15 = 1'b0;
  wire sn_raw0, sn_raw1, sn_raw2, sn_raw3;

  jt89 sn76489 (
      .rst(chip_reset),
      .clk(clk),
      .clk_en(cen_sn),
      .wr_n(sn_wr_n),
      .cs_n(1'b0),
      .din(sn_din),
      .fb_mask(sn_fb_mask),
      .sr15(sn_sr15),
      .sound(sn_sound),
      .raw0(sn_raw0),
      .raw1(sn_raw1),
      .raw2(sn_raw2),
      .raw3(sn_raw3),
      .ready()
  );

  // --------------------------------------------------------------------
  // OPL3 (opl3_fpga, LGPL): регистровый клок ~12.727 МГц; CDC хост-шины
  // встроен в host_if (afifo), выход забираем двойной регистрацией.
  reg [31:0] opl_half_phase = 0;
  reg opl_half_carry = 0;
  reg opl_clk_r = 0;

  always @(posedge clk) begin
    opl_half_carry <= 0;
    if (!pause_r) {opl_half_carry, opl_half_phase} <= {1'b0, opl_half_phase} + {1'b0, opl_phase_inc};
    if (opl_half_carry) opl_clk_r <= ~opl_clk_r;
  end

  wire opl_clk;
`ifdef M4_SIM
  assign opl_clk = opl_clk_r;
`else
  global opl_clk_buf (.in(opl_clk_r), .out(opl_clk));
`endif

  reg opl_cs_n = 1;
  reg opl_wr_n = 1;
  reg [1:0] opl_addr = 0;
  reg [7:0] opl_din = 0;
  wire signed [23:0] opl_l24;
  wire signed [23:0] opl_r24;

  opl3 opl3 (
      .clk(opl_clk),
      .clk_host(clk),
      .clk_dac(opl_clk),
      .ic_n(~chip_reset),
      .cs_n(opl_cs_n),
      .rd_n(1'b1),
      .wr_n(opl_wr_n),
      .address(opl_addr),
      .din(opl_din),
      .dout(),
      .sample_valid(),
      .sample_l(opl_l24),
      .sample_r(opl_r24),
      .led(),
      .irq_n()
  );

  // выход OPL3 выровнен вправо: берём >>3 с насыщением до 16 бит
  function automatic signed [15:0] sat16_21(input signed [20:0] v);
    sat16_21 = v > 32767 ? 16'd32767 : v < -32768 ? -16'd32768 : v[15:0];
  endfunction

  reg signed [15:0] opl_l_s1 = 0, opl_l_s = 0;
  reg signed [15:0] opl_r_s1 = 0, opl_r_s = 0;
  always @(posedge clk) begin
    opl_l_s1 <= sat16_21(opl_l24[23:3]);
    opl_l_s <= opl_l_s1;
    opl_r_s1 <= sat16_21(opl_r24[23:3]);
    opl_r_s <= opl_r_s1;
  end

  // --------------------------------------------------------------------
  // Выходной микс чипов: YM (знаковый, стерео) + AY (беззнаковый моно,
  // 10 бит << 5) + SegaPCM (знаковый, стерео) с насыщением до 16 бит.
  //
  // Частота строба — ровно частота выхода i2s, 48 кГц. Раньше здесь было
  // 55 кГц, и разницу закрывал audio.sv: он защёлкивает последний
  // выданный сэмпл и отдаёт его в i2s на 48 кГц, то есть 7029 сэмплов в
  // секунду просто выбрасывались. Такая выборка-с-удержанием рождает
  // НЕАРМОНИЧЕСКИЕ призраки на |55029-48000| +- f, и тем громче, чем
  // выше тон: тон 5 кГц давал 2029 Гц на -20 дБ, тон 8 кГц — 971 Гц на
  // -15 дБ. Уровень при этом падал на 0.1-0.6 дБ, поэтому сравнение с
  // эталоном по уровню, полосам и огибающей этого не показывало вообще.
  //
  // 57120000 / 48000 = 1190 ровно, поэтому каскад убирается целиком, а
  // не смягчается. clk_sys и audio_mclk идут от разных PLL, но от одного
  // кварца 74.25 МГц, так что остаётся расхождение в единицы-десятки ppm
  // — один повторённый или пропущенный сэмпл раз в доли секунды вместо
  // семи тысяч в секунду. Призрак от такого расхождения садится на
  // f +- (единицы Гц), то есть вырождается в медленную модуляцию.
  // Строб микса идёт ВДВОЕ чаще выхода, а выходной отсчёт — среднее двух
  // подотсчётов.
  //
  // Зачем. Чипы отдают сэмплы на своих частотах: jt12 при 7.67 МГц — на
  // 53267 Гц, а AY, APU, Game Boy, SID, HuC6280 и SN76489 вообще меняют
  // выход на МГц-овой сетке. Читать это на 48 кГц без сглаживания значит
  // заворачивать всё, что выше Найквиста, обратно в полосу. Измерено на
  // железе (scripts/image_check.py): тон 1848 Гц у YM2612 давал
  // НЕАРМОНИЧЕСКИЙ пик на |53267-48000| - 1848 = 3419 Гц на -17.1 дБ.
  // Гармоники при этом лежали на -29 дБ и ниже, то есть призрак был
  // громче любой из них.
  //
  // Усреднение двух подотсчётов — это КИХ длины 2 на частоте 96 кГц. Его
  // ноль стоит ровно на 48 кГц, а на 44581 Гц, откуда заворачивается
  // призрак, ослабление 19 дБ. Полезному сигналу на 1848 Гц он стоит
  // 0.02 дБ. Пять подотсчётов дали бы 23 дБ, но делить на 5 пришлось бы
  // умножителем, а площадь у нас на исходе (см. историю с K053260).
  //
  // 96 кГц выбрана как ровный делитель: 57120000 / 96000 = 595, и выход
  // остаётся точно 48 кГц — 57120000 / (2*595) = 48000.
  localparam SUB_DIV = CLK_HZ / 96_000;
  localparam SUB_CNT_W = $clog2(SUB_DIV);

  reg [SUB_CNT_W-1:0] sub_cnt = 0;
  reg sub_phase = 0;
  wire sub_tick = sub_cnt == SUB_CNT_W'(SUB_DIV - 1);

  wire signed [16:0] ay_wide = {2'b00, ay_sound, 5'b00000};
`ifdef M4_HAS_ARCADE
  wire signed [15:0] adpcm_wide = {adpcm_sound, 4'b0000};
`endif
  // VRC6 подмешивается к APU до общего DC-блокера (оба однополярные)
  wire [5:0] vrc6_out;

  vrc6 vrc6_i (
      .clk(clk),
      .cen(cen_nes),
      .rst(chip_reset || !vrc6_en),
      .wr(vrc6_wr),
      .blk(vrc6_blk),
      .rsel(vrc6_rsel),
      .din(vrc6_din),
      .out(vrc6_out)
  );

  wire signed [16:0] apu_wide = {2'b00, apu_sample[15:1]}
      + {3'b000, vrc6_out, 8'b0};  // выход APU беззнаковый, VRC6 0..61 << 8

  // DC-блокеры для однополярных источников (AY и NES APU держат
  // постоянное смещение): y[n] = x[n] - x[n-1] + y[n-1]*(1 - 2^-11),
  // обновление на подстробе 96 кГц => срез 96000/(2*pi*2048) = 7.5 Гц.
  // Блокеры обязаны идти на подстробе, а не на выходной частоте: иначе их
  // выход меняется только 48000 раз в секунду, усреднять нечего, и
  // сглаживание для этих чипов пропадает. Удвоение частоты вместе с
  // удвоением сдвига оставляет срез там же, где он был.
  reg signed [16:0] ay_hp_x = 0;
  reg signed [18:0] ay_hp_y = 0;
  reg signed [16:0] apu_hp_x = 0;
  reg signed [18:0] apu_hp_y = 0;

  function automatic signed [15:0] sat16_19(input signed [18:0] v);
    sat16_19 = v > 32767 ? 16'd32767 : v < -32768 ? -16'd32768 : v[15:0];
  endfunction

  reg signed [16:0] gbl_hp_x = 0;
  reg signed [18:0] gbl_hp_y = 0;
  reg signed [16:0] gbr_hp_x = 0;
  reg signed [18:0] gbr_hp_y = 0;
  wire signed [16:0] gbl_wide = {1'b0, gb_left_s};
  wire signed [16:0] gbr_wide = {1'b0, gb_right_s};

  reg signed [16:0] sid_hp_x = 0;
  reg signed [18:0] sid_hp_y = 0;
  // HuC6280 стерео: без блокера постоянная составляющая (канал с
  // ненаписанной таблицей даёт -16 на отсчёт) смещает сигнал от нуля,
  // съедает запас и упирается в потолок на громких местах — на тихих
  // при этом чисто. Остальные чипы через блокер шли с самого начала.
  reg signed [16:0] hucl_hp_x = 0;
  reg signed [18:0] hucl_hp_y = 0;
  reg signed [16:0] hucr_hp_x = 0;
  reg signed [18:0] hucr_hp_y = 0;

  wire signed [15:0] ay_hp = sat16_19(ay_hp_y);
  wire signed [15:0] apu_hp = sat16_19(apu_hp_y);
  wire signed [15:0] gbl_hp = sat16_19(gbl_hp_y);
  wire signed [15:0] gbr_hp = sat16_19(gbr_hp_y);
  wire signed [15:0] sid_hp = sat16_19(sid_hp_y);
  wire signed [15:0] hucl_hp = sat16_19(hucl_hp_y);
  wire signed [15:0] hucr_hp = sat16_19(hucr_hp_y);

  // Утечка фильтра: y>>>11, но не меньше +-1, иначе целочисленный хвост
  // «прилипает» и остаётся постоянный DC
  function automatic signed [18:0] hp_leak(input signed [18:0] y);
    hp_leak = (y >>> 11) != 0 ? (y >>> 11) : y > 0 ? 19'sd1 : y < 0 ? -19'sd1 : 19'sd0;
  endfunction
`ifdef M4_HAS_ARCADE
  wire signed [15:0] adpcm_l = adpcm_pan[0] ? 16'sd0 : adpcm_wide;
  wire signed [15:0] adpcm_r = adpcm_pan[1] ? 16'sd0 : adpcm_wide;
`endif

  // Взвешивание источников: (x * gain) >>> 6, gain в 1/64 долях
  wire signed [8:0] g_ym = {1'b0, mix_gains[7:0]};
  wire signed [8:0] g_ay = {1'b0, mix_gains[15:8]};
  wire signed [8:0] g_pcm = {1'b0, mix_gains[23:16]};
  wire signed [8:0] g_adpcm = {1'b0, mix_gains[31:24]};
  wire signed [8:0] g_apu = {1'b0, apu_gain};
  wire signed [8:0] g_gb = {1'b0, gb_gain};
  wire signed [8:0] g_sid = {1'b0, sid_gain};
  wire signed [8:0] g_opl = {1'b0, opl_gain};
  wire signed [8:0] g_fm = {1'b0, fm_gain};
  wire signed [8:0] g_sn = {1'b0, sn_gain};
  // --------------------------------------------------------------------
  // Стерео SN76489.
  //
  // Зачем отдельный путь. У T6W28 (Neo Geo Pocket) две банки аттенюаторов,
  // по одной на сторону, а мы сводили их в моно по громкой — голоса,
  // отведённые вправо, теряли свою громкость. У Game Gear стороны задаются
  // маской, и её мы вообще не воспроизводили.
  //
  // Как сделано. jt89 отдаёт сырые квадраты каналов до применения
  // громкости (raw0..raw3), а аттенюация накладывается здесь, своя на
  // каждую сторону. Таблица — та же, что в jt89_vol и в описании чипа:
  // шаг 2 дБ, пятнадцатая ступень это тишина.
  //
  // Путь включается фирмварью только для файлов, которые стерео объявили.
  // Обычные моно-файлы идут прежней дорогой через sn_sound бит в бит: при
  // равных аттенюациях на сторонах суммы совпадают по построению.
  reg sn_stereo_en = 0;
  reg [15:0] sn_att_l = {4{4'hF}};  // по 4 бита на канал, 15 = тишина
  reg [15:0] sn_att_r = {4{4'hF}};

  function automatic signed [8:0] sn_ch(input raw, input [3:0] att);
    reg [7:0] mx;
    begin
      case (att)
        4'd0:  mx = 8'd255; 4'd1:  mx = 8'd203; 4'd2:  mx = 8'd161; 4'd3:  mx = 8'd128;
        4'd4:  mx = 8'd102; 4'd5:  mx = 8'd81;  4'd6:  mx = 8'd64;  4'd7:  mx = 8'd51;
        4'd8:  mx = 8'd40;  4'd9:  mx = 8'd32;  4'd10: mx = 8'd26;  4'd11: mx = 8'd20;
        4'd12: mx = 8'd16;  4'd13: mx = 8'd13;  4'd14: mx = 8'd10;  default: mx = 8'd0;
      endcase
      sn_ch = raw ? {1'b0, mx} : -{1'b0, mx};
    end
  endfunction

  wire signed [10:0] sn_sum_l = sn_ch(sn_raw0, sn_att_l[3:0]) + sn_ch(sn_raw1, sn_att_l[7:4])
                              + sn_ch(sn_raw2, sn_att_l[11:8]) + sn_ch(sn_raw3, sn_att_l[15:12]);
  wire signed [10:0] sn_sum_r = sn_ch(sn_raw0, sn_att_r[3:0]) + sn_ch(sn_raw1, sn_att_r[7:4])
                              + sn_ch(sn_raw2, sn_att_r[11:8]) + sn_ch(sn_raw3, sn_att_r[15:12]);

  wire signed [15:0] sn_wide_l = {sn_stereo_en ? sn_sum_l : sn_sound, 5'b00000};
  wire signed [15:0] sn_wide_r = {sn_stereo_en ? sn_sum_r : sn_sound, 5'b00000};
`ifdef M4_HAS_HOME
  // SCC моно, знаковый 11 бит -> << 5 (x32) до 16 бит
  wire signed [8:0] g_scc = {1'b0, scc_gain};
  wire signed [15:0] scc_wide = {scc_sound, 5'b00000};
  // HuC6280 стерео, знаковый 16 бит
  wire signed [8:0] g_huc = {1'b0, huc_gain};
`endif
`ifdef M4_HAS_ARCADE
  // OKIM6295 моно, знаковый 14 бит -> << 2 (x4) до 16 бит
  wire signed [8:0] g_okim = {1'b0, okim_gain};
  wire signed [15:0] okim_wide = {okim_sound, 2'b00};
  // K053260 стерео, знаковый 16 бит
  wire signed [8:0] g_k060 = {1'b0, k060_gain};
`endif
`ifdef M4_HAS_HOME
  wire signed [15:0] sid_wide = sid_audio[17:2];
  wire signed [16:0] sid_wide17 = {sid_wide[15], sid_wide};
`endif

  // Конвейер: произведения регистрируются (разгрузка длинного пути),
  // сумма — на следующем такте; строб выхода ~55 кГц задержки не заметит
  reg signed [25:0] ay_g;
  reg signed [25:0] apu_g, opl_l_g, opl_r_g;
`ifdef M4_HAS_HOME
  reg signed [25:0] gbl_g, gbr_g, sid_g;
`else
  reg signed [25:0] gbl_g = 0, gbr_g = 0, sid_g = 0;
`endif
`ifdef M4_HAS_ARCADE
  reg signed [25:0] ym_l_g, ym_r_g, pcm_l_g, pcm_r_g, adpcm_l_g, adpcm_r_g;
`else
  reg signed [25:0] ym_l_g = 0, ym_r_g = 0, pcm_l_g = 0, pcm_r_g = 0;
  reg signed [25:0] adpcm_l_g = 0, adpcm_r_g = 0;
`endif
  reg signed [25:0] fm_l_g, fm_r_g, sn_l_g, sn_r_g;
  // SCC/OKIM6295/K053260 — только в симуляции; в Quartus = 0
  reg signed [25:0] scc_g = 0, okim_g = 0, k060_l_g = 0, k060_r_g = 0;
  reg signed [25:0] huc_l_g = 0, huc_r_g = 0;

  always @(posedge clk) begin
    ay_g <= (ay_hp * g_ay) >>> 6;
`ifdef M4_HAS_ARCADE
    ym_l_g <= (ym_l * g_ym) >>> 6;
    ym_r_g <= (ym_r * g_ym) >>> 6;
    pcm_l_g <= (pcm_l * g_pcm) >>> 6;
    pcm_r_g <= (pcm_r * g_pcm) >>> 6;
    adpcm_l_g <= (adpcm_l * g_adpcm) >>> 6;
    adpcm_r_g <= (adpcm_r * g_adpcm) >>> 6;
`endif
    apu_g <= (apu_hp * g_apu) >>> 6;
`ifdef M4_HAS_HOME
    gbl_g <= (gbl_hp * g_gb) >>> 6;
    gbr_g <= (gbr_hp * g_gb) >>> 6;
`endif
`ifdef M4_HAS_HOME
    sid_g <= (sid_hp * g_sid) >>> 6;
`endif
    opl_l_g <= (opl_l_s * g_opl) >>> 6;
    opl_r_g <= (opl_r_s * g_opl) >>> 6;
    fm_l_g <= (fm_l * g_fm) >>> 6;
    fm_r_g <= (fm_r * g_fm) >>> 6;
    sn_l_g <= (sn_wide_l * g_sn) >>> 6;
    sn_r_g <= (sn_wide_r * g_sn) >>> 6;
`ifdef M4_HAS_HOME
    scc_g <= (scc_wide * g_scc) >>> 6;
    huc_l_g <= (hucl_hp * g_huc) >>> 6;
    huc_r_g <= (hucr_hp * g_huc) >>> 6;
`endif
`ifdef M4_HAS_ARCADE
    okim_g <= (okim_wide * g_okim) >>> 6;
    k060_l_g <= (k060_l * g_k060) >>> 6;
    k060_r_g <= (k060_r * g_k060) >>> 6;
`endif
  end

  wire signed [25:0] mix_l = ym_l_g + ay_g + pcm_l_g + adpcm_l_g + apu_g + gbl_g + sid_g + opl_l_g + fm_l_g + sn_l_g + scc_g + huc_l_g + okim_g + k060_l_g;
  wire signed [25:0] mix_r = ym_r_g + ay_g + pcm_r_g + adpcm_r_g + apu_g + gbr_g + sid_g + opl_r_g + fm_r_g + sn_r_g + scc_g + huc_r_g + okim_g + k060_r_g;

  function automatic signed [15:0] sat16(input signed [25:0] v);
    sat16 = v > 32767 ? 16'd32767 : v < -32768 ? -16'd32768 : v[15:0];
  endfunction

`ifdef M4_HAS_HOME
  // --------------------------------------------------------------------
  // Выходной фильтр Mega Drive.
  //
  // У консоли в аналоговом тракте стоит фильтр нижних частот, и он есть у
  // ОБЕИХ моделей, а не только у второй: первая режет около 3.1 кГц,
  // вторая около 3.96 кГц. Ни у нас, ни у эталонной эмуляции его не было,
  // поэтому по полосам мы с эталоном сходились, а на слух звучали ярче
  // настоящей приставки — верх не заваливался, и звуки казались
  // звенящими.
  //
  // Модель и коэффициенты сняты с genesis_lpf.v ядра Genesis для MiSTer
  // (Gregory Hogan, MIT): разностное уравнение
  //     y[n] = (B1*x[n-1] + B2*x[n-2] - A2*y[n-1]) >> 15
  // Там она считается на 106528 Гц, у нас строб микса 48000 Гц, поэтому
  // перенесены не числа, а полюс и ноль: p' = p^(fs1/fs2), и коэффициенты
  // пересчитаны так, чтобы усиление на постоянном токе осталось единичным
  // (B1 + B2 - A2 = 32768). Ноль у первой модели не совпадает с полюсом —
  // это не чистый Butterworth, а подогнанный под измеренную АЧХ спад: в
  // аналоговых терминах это полка, полюс 2.97 кГц и ноль 11.8 кГц.
  //
  // Числа пересчитаны со строба 55029 Гц на 48000 (scratchpad/lpf_48k.py).
  // У Model 1 и Model 2 АЧХ совпадает с прежней до 0.06 дБ во всей
  // полосе. У «минимального» ноль стоит на Найквисте, а Найквист уехал
  // с 27.5 на 24 кГц, поэтому выше 10 кГц он теперь режет на 0.4-1.1 дБ
  // больше — перенести и полку, и положение нуля одновременно нельзя.
  //
  // Режим ставит фирмварь: он нужен файлам Mega Drive, но не OPN-рипам
  // с PC-98, которые ходят через тот же jt12. 3 = фильтр выключен.
  reg [1:0] md_lpf_mode = 2'd3;
  reg signed [17:0] lpf_a2, lpf_b1, lpf_b2;
  always @(*) begin
    case (md_lpf_mode)
      2'd0: begin lpf_a2 = -18'sd22215; lpf_b1 = 18'sd13439; lpf_b2 = -18'sd2886; end
      2'd1: begin lpf_a2 = -18'sd20163; lpf_b1 = 18'sd16046; lpf_b2 = -18'sd3441; end
      2'd2: begin lpf_a2 = -18'sd9876; lpf_b1 = 18'sd11447; lpf_b2 = 18'sd11445; end
      default: begin lpf_a2 = 18'sd0; lpf_b1 = 18'sd0; lpf_b2 = 18'sd0; end
    endcase
  end

  reg signed [15:0] lpf_x0_l = 0, lpf_x1_l = 0, lpf_y_l = 0;
  reg signed [15:0] lpf_x0_r = 0, lpf_x1_r = 0, lpf_y_r = 0;
  wire signed [33:0] lpf_acc_l = lpf_b1 * lpf_x0_l + lpf_b2 * lpf_x1_l - lpf_a2 * lpf_y_l;
  wire signed [33:0] lpf_acc_r = lpf_b1 * lpf_x0_r + lpf_b2 * lpf_x1_r - lpf_a2 * lpf_y_r;

  function automatic signed [15:0] sat16_34(input signed [33:0] v);
    // 16'sh8000 вместо -16'sd32768: литерал 16'sd32768 в знаковые 16 бит
    // не влезает, и Quartus ругается «constant value overflow». Значение
    // получалось верное только по счастливой случайности.
    sat16_34 = v > 32767 ? 16'sd32767 : v < -32768 ? 16'sh8000 : v[15:0];
  endfunction

  // --------------------------------------------------------------------
  // Выходной тракт NES.
  //
  // За ЦАП-ами приставки стоит цепочка, задокументированная на NESdev:
  // ФВЧ первого порядка 90 Гц, второй ФВЧ 440 Гц и ФНЧ 14 кГц. Так
  // считают Mesen, Nestopia и puNES. У нас на этом месте был только
  // общий DC-блокер на 7.5 Гц, то есть низа у нас заметно больше, чем у
  // приставки: каскад 90+440 режет 110 Гц на 14.5 дБ, 220 Гц на 7.7 дБ.
  //
  // Почему режимами, а не молча. Эталоны расходятся: libgme, по которому
  // мы сверяемся, применяет вместо этой цепочки мягкий спад от 80 Гц, и
  // потому провала не показывал. У Famicom тракт свой и второго ФВЧ в
  // нём нет. Решать на слух — владельцу, как это уже вышло с фильтром
  // Mega Drive. 0 = NES, 1 = Famicom (без 440 Гц), 3 = выключено.
  reg [1:0] nes_flt_mode = 2'd3;

  // Состояние держим в 16.8 — младшие 8 бит дробные. Без них целое
  // усечение «прилипает»: у ФВЧ при y = -1 арифметический сдвиг даёт
  // снова -1, и фильтр вместо затухания звенит между -1 и +1.
  localparam NF_W = 27;
  reg signed [NF_W-1:0] nf_h1x_l = 0, nf_h1y_l = 0, nf_h2x_l = 0, nf_h2y_l = 0, nf_lp_l = 0;
  reg signed [NF_W-1:0] nf_h1x_r = 0, nf_h1y_r = 0, nf_h2x_r = 0, nf_h2y_r = 0, nf_lp_r = 0;

  // Коэффициенты подобраны так, чтобы обойтись сдвигами и сложениями:
  // умножители в проекте на исходе. a = 1 - 3/256 даёт срез 90.6 Гц,
  // a = 1 - 7/128 — 442 Гц, b = 165/256 — 13.9 кГц. Отклонение от
  // задокументированных 90 / 440 / 14000 меньше процента.
  function automatic signed [NF_W-1:0] nf_hp90(input signed [NF_W-1:0] t);
    nf_hp90 = t - ((t * 3 + 128) >>> 8);
  endfunction
  function automatic signed [NF_W-1:0] nf_hp440(input signed [NF_W-1:0] t);
    nf_hp440 = t - ((t * 7 + 64) >>> 7);
  endfunction
  function automatic signed [NF_W-1:0] nf_lp14k(input signed [NF_W-1:0] d);
    nf_lp14k = (d * 165 + 128) >>> 8;
  endfunction
  function automatic signed [15:0] sat16_nf(input signed [NF_W-1:0] v);
    sat16_nf = (v >>> 8) > 32767 ? 16'sd32767
             : (v >>> 8) < -32768 ? 16'sh8000 : v[23:8];  // см. sat16_34
  endfunction
`endif

`ifdef M4_HAS_HOME
  // Выход тракта Mega Drive — он же вход тракта NES
  wire signed [15:0] md_out_l = md_lpf_mode == 2'd3 ? sat16(avg_l) : lpf_y_l;
  wire signed [15:0] md_out_r = md_lpf_mode == 2'd3 ? sat16(avg_r) : lpf_y_r;
  // Вход в 16.8 и обе ступени ФВЧ. Второй ФВЧ считается всегда, а в
  // режиме Famicom просто не выбирается — так его состояние не
  // застаивается и переключение не даёт щелчка длиной в постоянную времени.
  wire signed [NF_W-1:0] nf_in_l = {{(NF_W - 24) {md_out_l[15]}}, md_out_l, 8'b0};
  wire signed [NF_W-1:0] nf_in_r = {{(NF_W - 24) {md_out_r[15]}}, md_out_r, 8'b0};
  wire signed [NF_W-1:0] nf_s1_l = nf_hp90(nf_h1y_l + nf_in_l - nf_h1x_l);
  wire signed [NF_W-1:0] nf_s1_r = nf_hp90(nf_h1y_r + nf_in_r - nf_h1x_r);
  wire signed [NF_W-1:0] nf_s2_l = nf_hp440(nf_h2y_l + nf_s1_l - nf_h2x_l);
  wire signed [NF_W-1:0] nf_s2_r = nf_hp440(nf_h2y_r + nf_s1_r - nf_h2x_r);
  wire signed [NF_W-1:0] nf_pick_l = nes_flt_mode == 2'd0 ? nf_s2_l : nf_s1_l;
  wire signed [NF_W-1:0] nf_pick_r = nes_flt_mode == 2'd0 ? nf_s2_r : nf_s1_r;
`endif

  // Накопитель усреднения: держит первый подотсчёт до второго
  reg signed [25:0] acc_l = 0, acc_r = 0;
  wire signed [26:0] sum2_l = {mix_l[25], mix_l} + {acc_l[25], acc_l};
  wire signed [26:0] sum2_r = {mix_r[25], mix_r} + {acc_r[25], acc_r};
  // деление на 2 знакового числа — это взятие старших разрядов суммы
  wire signed [25:0] avg_l = sum2_l[26:1];
  wire signed [25:0] avg_r = sum2_r[26:1];

  always @(posedge clk) begin
    if (sub_tick) begin
      sub_cnt <= 0;
      sub_phase <= ~sub_phase;
      ay_hp_y <= (ay_wide - ay_hp_x) + (ay_hp_y - hp_leak(ay_hp_y));
      ay_hp_x <= ay_wide;
      apu_hp_y <= (apu_wide - apu_hp_x) + (apu_hp_y - hp_leak(apu_hp_y));
      apu_hp_x <= apu_wide;
`ifdef M4_HAS_HOME
      gbl_hp_y <= (gbl_wide - gbl_hp_x) + (gbl_hp_y - hp_leak(gbl_hp_y));
      gbl_hp_x <= gbl_wide;
      gbr_hp_y <= (gbr_wide - gbr_hp_x) + (gbr_hp_y - hp_leak(gbr_hp_y));
      gbr_hp_x <= gbr_wide;
      sid_hp_y <= (sid_wide17 - sid_hp_x) + (sid_hp_y - hp_leak(sid_hp_y));
      sid_hp_x <= sid_wide17;
      hucl_hp_y <= (huc_left - hucl_hp_x) + (hucl_hp_y - hp_leak(hucl_hp_y));
      hucl_hp_x <= huc_left;
      hucr_hp_y <= (huc_right - hucr_hp_x) + (hucr_hp_y - hp_leak(hucr_hp_y));
      hucr_hp_x <= huc_right;
`endif
      // Первый подотсчёт только запоминается, на втором выдаётся среднее.
      // Фильтр Mega Drive и выход идут на 48 кГц, как и раньше, поэтому
      // его коэффициенты остаются теми же.
      if (!sub_phase) begin
        acc_l <= mix_l;
        acc_r <= mix_r;
      end else begin
`ifdef M4_HAS_HOME
        // Фильтр Mega Drive: y считается по прежним x0/x1/y — как в
        // исходной модели, где новый отсчёт попадает в x0 в этом же такте.
        lpf_y_l <= sat16_34(lpf_acc_l >>> 15);
        lpf_x1_l <= lpf_x0_l;
        lpf_x0_l <= sat16(avg_l);
        lpf_y_r <= sat16_34(lpf_acc_r >>> 15);
        lpf_x1_r <= lpf_x0_r;
        lpf_x0_r <= sat16(avg_r);
        // Тракт NES стоит за трактом Mega Drive. Оба сразу не включаются
        // никогда: режим ставит фирмварь по тому, какой это файл.
        nf_h1x_l <= nf_in_l;
        nf_h1y_l <= nf_s1_l;
        nf_h2x_l <= nf_s1_l;
        nf_h2y_l <= nf_s2_l;
        nf_lp_l <= nf_lp_l + nf_lp14k(nf_pick_l - nf_lp_l);
        nf_h1x_r <= nf_in_r;
        nf_h1y_r <= nf_s1_r;
        nf_h2x_r <= nf_s1_r;
        nf_h2y_r <= nf_s2_r;
        nf_lp_r <= nf_lp_r + nf_lp14k(nf_pick_r - nf_lp_r);
        chip_left <= nes_flt_mode == 2'd3 ? md_out_l : sat16_nf(nf_lp_l);
        chip_right <= nes_flt_mode == 2'd3 ? md_out_r : sat16_nf(nf_lp_r);
`else
        chip_left <= sat16(avg_l);
        chip_right <= sat16(avg_r);
`endif
        chip_sample_toggle <= ~chip_sample_toggle;
      end
    end else begin
      sub_cnt <= sub_cnt + 1'b1;
    end

    // пульсы доменов: смена выходного сэмпла GB/OPL
    gb_beat_prev <= gb_left_s;
    opl_beat_prev <= opl_l_s;
    if (gb_left_s != gb_beat_prev) gb_beat <= gb_beat + 1'b1;
    if (opl_l_s != opl_beat_prev) opl_beat <= opl_beat + 1'b1;
    if (soft_reset_req) begin
      gb_beat <= 0;
      opl_beat <= 0;
    end

    // VU-пики по зарегистрированному выходу (лаг в 1 сэмпл не важен)
    if (vu_take_d != vu_take) begin
      vu_l <= 0;
      vu_r <= 0;
      vu_take_d <= vu_take;
    end else begin
      if (vu_abs_l > vu_l) vu_l <= vu_abs_l;
      if (vu_abs_r > vu_r) vu_r <= vu_abs_r;
    end
  end

  wire [15:0] vu_abs_l = chip_left[15] ? -chip_left : chip_left;
  wire [15:0] vu_abs_r = chip_right[15] ? -chip_right : chip_right;

  // --------------------------------------------------------------------
  // Секвенсор.
  //
  // Время ведётся парой tick_count (свободно бежит) / tick_target
  // (наращивается командами WAIT): команды исполняются, пока цель не
  // впереди счётчика. Так длительность записей в чип не накапливает
  // дрейф — опоздавшая запись лишь сдвигает фазу, а не темп. Если FIFO
  // пуст (пауза/недокорм), цель ресинкается на текущий счётчик.
  localparam OP_YM2151 = 4'h1;
  localparam OP_AY = 4'h2;
  localparam OP_PCM = 4'h3;
  localparam OP_ADPCM = 4'h4;
  localparam OP_STR_ADDR = 4'h5;
  localparam OP_STR_START = 4'h6;
  localparam OP_STR_STOP = 4'h7;
  localparam OP_WAIT = 4'h8;
  localparam OP_APU = 4'h9;
  localparam OP_NESRAM_PTR = 4'hA;
  localparam OP_NESRAM_WR = 4'hB;
  localparam OP_OPL3 = 4'hC;
  localparam OP_FM2612 = 4'hD;
  localparam OP_SN = 4'hE;
  localparam OP_EXT = 4'hF;   // расширенные чипы: суб-код в fifo_q[27:24]
  localparam EXT_SCC = 4'h0;  // Konami SCC/K051649
  localparam EXT_OKIM = 4'h1; // OKIM6295
  localparam EXT_K060 = 4'h2; // Konami K053260
  localparam EXT_HUC  = 4'h3; // HuC6280 PSG (PC Engine)
  localparam EXT_GB   = 4'h4; // прямая запись в APU Game Boy (VGM)
  localparam EXT_SNST = 4'h5; // аттенюации SN76489 по сторонам (стерео)

  localparam S_IDLE = 4'd0;
  localparam S_DECODE = 4'd1;
  localparam S_POLL_A = 4'd2;
  localparam S_WR_A = 4'd3;
  localparam S_POLL_D = 4'd4;
  localparam S_WR_D = 4'd5;
  localparam S_AY_WR = 4'd6;
  localparam S_ADPCM_POLL = 4'd7;
  localparam S_ADPCM_WR = 4'd8;
  localparam S_APU_WR = 4'd9;
  localparam S_NESRAM = 4'd10;
  localparam S_OPL_A = 4'd11;
  localparam S_OPL_GAP = 4'd12;
  localparam S_OPL_D = 4'd13;
  localparam S_OPL_END = 4'd14;
  localparam S_G_POLL_A = 5'd16;
  localparam S_G_WR_A = 5'd17;
  localparam S_G_POLL_D = 5'd18;
  localparam S_G_WR_D = 5'd19;
  localparam S_SN_WR = 5'd20;
  localparam S_GBEXT = 5'd25;  // выдержка после записи в APU Game Boy
  localparam S_SCC_A = 5'd21;  // строб SCC ассерчен, ждём cen_scc
  localparam S_SCC_Z = 5'd22;  // строб снят, ждём восстановления
  localparam S_OKIM = 5'd23;   // импульс wrn OKIM6295 (один такт)
  localparam S_K060_A = 5'd24; // строб K053260 ассерчен, ждём cen
  localparam S_K060_Z = 5'd25; // строб снят

  // Отображение (порт VGM 0xD2, регистр aa) -> адрес MSX ABLO (0x9800+)
  function automatic [7:0] scc_ablo_map(input [2:0] port, input [7:0] r);
    case (port)
      3'd0: scc_ablo_map = r;             // waveform 0x00-0x7F
      3'd1: scc_ablo_map = 8'h80 + r;     // frequency
      3'd2: scc_ablo_map = 8'h8A + r;     // volume
      3'd3: scc_ablo_map = 8'h8F;         // key on/off
      3'd4: scc_ablo_map = r;             // SCC-I 5-й канал (прибл.)
      3'd5: scc_ablo_map = 8'hE0 + r;     // test/deformation
      default: scc_ablo_map = r;
    endcase
  endfunction

  reg fm_port_l = 0;

  reg [1:0] opl_phase_cnt = 0;
  reg [7:0] opl_reg_l = 0;
  reg [7:0] opl_val_l = 0;
  reg opl_bank_l = 0;

  reg [4:0] state = S_IDLE;
  reg [3:0] scc_wait = 0;
  reg [14:0] nesram_ptr = 0;
  reg fsm_wr_req = 0;
  reg [14:0] fsm_wr_addr = 0;
  reg [7:0] fsm_wr_data = 0;
  reg [31:0] tick_count = 0;
  reg [31:0] tick_target = 0;
  reg [31:0] fifo_q = 0;
  reg [15:0] poll_guard = 0;
  reg [7:0] cur_reg = 0;
  reg [7:0] cur_val = 0;

  wire [31:0] tick_diff = tick_target - tick_count;
  wire time_pending = !tick_diff[31] && tick_diff != 0;
  wire seq_busy = state != S_IDLE || time_pending || !fifo_empty;

  always @(posedge clk) begin
`ifdef M4_HAS_HOME
    huc_wr <= 0;   // однотактовый строб: гасится там же, где взводится
`endif
    if (reset || soft_reset_req) begin
      rd_ptr <= 0;
      state <= S_IDLE;
      tick_count <= 0;
      tick_target <= 0;
`ifdef M4_HAS_ARCADE
      ym_cs_n <= 1;
      ym_wr_n <= 1;
      pcm_cs <= 0;
      adpcm_fsm_wr <= 0;
      adpcm_pan <= 0;
      str_set_addr <= 0;
      str_start <= 0;
      str_stop <= 0;
`endif
      ay_cs_n <= 1;
      ay_wr_n <= 1;
      apu_cs <= 0;
      fsm_wr_req <= 0;
`ifdef M4_HAS_HOME
      scc_cs_n <= 1;
      scc_wr_n <= 1;
`endif
`ifdef M4_HAS_ARCADE
      okim_wrn <= 1;
      k060_cs <= 0;
      k060_wr_n <= 1;
`endif
      opl_cs_n <= 1;
      opl_wr_n <= 1;
      fm_cs_n <= 1;
      fm_wr_n <= 1;
      sn_wr_n <= 1;
    end else begin
`ifdef M4_HAS_ARCADE
      str_set_addr <= 0;
      str_start <= 0;
      str_stop <= 0;
`endif
      fsm_wr_req <= 0;
      if (tick) tick_count <= tick_count + 1'b1;

      case (state)
        S_IDLE: begin
`ifdef M4_HAS_ARCADE
          ym_cs_n <= 1;
          ym_wr_n <= 1;
`endif
          if (fifo_empty) begin
            if (!time_pending) tick_target <= tick_count;
          end else if (tick_diff[31] && (tick_count - tick_target) > 32'd2048) begin
            // Ограничение отставания цели. При перемотке tick_count идёт
            // в 8 раз быстрее, а цель растёт номинальными длительностями
            // — счётчик уходит вперёд, и отставание копится, пока держат
            // кнопку. Ресинк выше не помогает: он ждёт пустой FIFO, а при
            // перемотке FIFO полон. После отпускания бит снимался сразу,
            // но все ожидания отрабатывали мгновенно, пока цель не
            // догонит счётчик — музыка неслась ещё секунду-две.
            // Порог 2048 тиков (~46 мс при 44.1 кГц) переживает обычный
            // джиттер и не слышен.
            tick_target <= tick_count;
          end else if (!time_pending && !chip_reset) begin
            fifo_q <= fifo_mem[rd_ptr[FIFO_AW-1:0]];
            rd_ptr <= rd_ptr + 1'b1;
            state  <= S_DECODE;
          end
        end

        S_DECODE: begin
          state <= S_IDLE;
          case (fifo_q[31:28])
`ifdef M4_HAS_ARCADE
            OP_YM2151: begin
              cur_reg <= fifo_q[15:8];
              cur_val <= fifo_q[7:0];
              poll_guard <= 16'hFFFF;
              state <= S_POLL_A;
            end
`endif
            OP_AY: begin
              // jt49 защёлкивает на любом такте — один строб без busy
              ay_addr <= fifo_q[11:8];
              ay_din <= fifo_q[7:0];
              ay_cs_n <= 0;
              ay_wr_n <= 0;
              state <= S_AY_WR;
            end
`ifdef M4_HAS_ARCADE
            OP_PCM: begin
              // регистры SegaPCM — двупортовая RAM, пишется на clk
              pcm_addr <= fifo_q[15:8];
              pcm_din <= fifo_q[7:0];
              pcm_cs <= 1;
              state <= S_AY_WR;  // общий однотактный строб-стейт
            end
            OP_ADPCM: begin
              if (fifo_q[9:8] == 2'd2) begin
                adpcm_pan <= fifo_q[1:0];  // пан — регистр микса, не чипа
              end else begin
                adpcm_fsm_addr <= fifo_q[9:8];
                adpcm_fsm_din <= fifo_q[7:0];
                state <= S_ADPCM_POLL;
              end
            end
            OP_STR_ADDR: begin
              str_payload <= fifo_q[23:0];
              str_set_addr <= 1;
            end
            OP_STR_START: begin
              str_payload <= fifo_q[23:0];
              str_start <= 1;
            end
            OP_STR_STOP: str_stop <= 1;
`endif
            OP_WAIT: tick_target <= tick_target + fifo_q[23:0];
            OP_APU: begin
              apu_addr <= fifo_q[12:8];
              apu_din <= fifo_q[7:0];
              apu_cs <= 1;
              state <= S_APU_WR;
            end
            OP_NESRAM_PTR: nesram_ptr <= fifo_q[14:0];
            OP_NESRAM_WR: begin
              fsm_wr_addr <= nesram_ptr;
              fsm_wr_data <= fifo_q[7:0];
              nesram_ptr <= nesram_ptr + 15'd1;
              state <= S_NESRAM;
            end
            OP_FM2612: begin
              fm_port_l <= fifo_q[16];
              cur_reg <= fifo_q[15:8];
              cur_val <= fifo_q[7:0];
              poll_guard <= 16'hFFFF;
              state <= S_G_POLL_A;
            end
            OP_SN: begin
              sn_din <= fifo_q[7:0];
              sn_wr_n <= 0;
              state <= S_SN_WR;
            end
            OP_OPL3: begin
              opl_bank_l <= fifo_q[16];
              opl_reg_l <= fifo_q[15:8];
              opl_val_l <= fifo_q[7:0];
              opl_addr <= fifo_q[16] ? 2'd2 : 2'd0;
              opl_din <= fifo_q[15:8];
              opl_cs_n <= 0;
              opl_wr_n <= 0;
              opl_phase_cnt <= 2;
              state <= S_OPL_A;
            end
            OP_EXT: begin
              case (fifo_q[27:24])
`ifdef M4_HAS_HOME
                EXT_GB: begin
                  gb_ext_addr <= fifo_q[15:8];
                  gb_ext_data <= fifo_q[7:0];
                  gb_ext_toggle <= ~gb_ext_toggle;
                  gb_ext_wait <= 7'd127;
                  state <= S_GBEXT;
                end
                EXT_HUC: begin
                  huc_addr <= fifo_q[11:8];
                  huc_din  <= fifo_q[7:0];
                  huc_wr   <= 1;
                end
                // Стерео SN76489: аттенюации идут ЧЕРЕЗ ОЧЕРЕДЬ, а не
                // прямой записью в регистр — иначе панорама опережала бы
                // музыку на всю глубину очереди, а это десятки миллисекунд.
                EXT_SNST: begin
                  if (fifo_q[16]) sn_att_r <= fifo_q[15:0];
                  else sn_att_l <= fifo_q[15:0];
                end
                EXT_SCC: begin
                  // порт 7 = разблокировка BR2 (ABHI=0x12, DB=0x3F)
                  if (fifo_q[18:16] == 3'd7) begin
                    scc_abhi <= 5'h12;
                    scc_ablo <= 8'h00;
                    scc_db   <= 8'h3F;
                  end else begin
                    scc_abhi <= 5'h13;
                    scc_ablo <= scc_ablo_map(fifo_q[18:16], fifo_q[15:8]);
                    scc_db   <= fifo_q[7:0];
                  end
                  scc_cs_n <= 0;
                  scc_wr_n <= 0;
                  scc_wait <= 0;
                  state <= S_SCC_A;
                end
`endif
`ifdef M4_HAS_ARCADE
                EXT_OKIM: begin
                  // один байт команды в OKIM6295: импульс wrn 1->0
                  okim_din <= fifo_q[7:0];
                  okim_wrn <= 0;
                  state <= S_OKIM;
                end
`endif
`ifdef M4_HAS_ARCADE
                EXT_K060: begin
                  // запись регистра K053260: addr[13:8], data[7:0]
                  k060_addr <= fifo_q[13:8];
                  k060_din <= fifo_q[7:0];
                  k060_cs <= 1;
                  k060_wr_n <= 0;
                  scc_wait <= 0;
                  state <= S_K060_A;
                end
`endif
                default: ;
              endcase
            end
            default: ;  // незнакомый опкод — пропускаем
          endcase
        end

        // Запись в APU: держим CS через фронт PHI2 (там латчится write)
        S_APU_WR: begin
          if (nes_phi2 && !nes_phi2_d) begin
            apu_cs <= 0;
            state <= S_IDLE;
          end
        end

        // OPL3: запись адресного порта, зазор, запись данных
        S_OPL_A: begin
          if (opl_phase_cnt != 0) opl_phase_cnt <= opl_phase_cnt - 1'b1;
          else begin
            opl_cs_n <= 1;
            opl_wr_n <= 1;
            opl_phase_cnt <= 3;
            state <= S_OPL_GAP;
          end
        end
        S_OPL_GAP: begin
          if (opl_phase_cnt != 0) opl_phase_cnt <= opl_phase_cnt - 1'b1;
          else begin
            opl_addr <= 2'd1;
            opl_din <= opl_val_l;
            opl_cs_n <= 0;
            opl_wr_n <= 0;
            opl_phase_cnt <= 2;
            state <= S_OPL_D;
          end
        end
        S_OPL_D: begin
          if (opl_phase_cnt != 0) opl_phase_cnt <= opl_phase_cnt - 1'b1;
          else begin
            opl_cs_n <= 1;
            opl_wr_n <= 1;
            state <= S_IDLE;
          end
        end

        // YM2612: busy-протокол как у jt51 (адрес, затем данные)
        S_G_POLL_A, S_G_POLL_D: begin
          fm_cs_n <= 0;
          fm_wr_n <= 1;
          poll_guard <= poll_guard - 1'b1;
          if (!fm_busy || poll_guard == 0) begin
            fm_addr <= state == S_G_POLL_D ? {fm_port_l, 1'b1} : {fm_port_l, 1'b0};
            fm_din <= state == S_G_POLL_A ? cur_reg : cur_val;
            fm_cs_n <= 0;
            fm_wr_n <= 0;
            state <= state == S_G_POLL_A ? S_G_WR_A : S_G_WR_D;
          end
        end

        S_G_WR_A, S_G_WR_D: begin
          if (cen_fm) begin
            fm_cs_n <= 1;
            fm_wr_n <= 1;
            if (state == S_G_WR_A) begin
              poll_guard <= 16'hFFFF;
              state <= S_G_POLL_D;
            end else begin
              state <= S_IDLE;
            end
          end
        end

        // SN76489: держим строб до cen включительно
        S_SN_WR: begin
          if (cen_sn) begin
            sn_wr_n <= 1;
            state <= S_IDLE;
          end
        end

`ifdef M4_HAS_HOME
        // SCC: держим CS/WR ассерченными 2 такта cen_scc (чип ловит
        // переход 1->0 по своему клоку), затем снимаем и ждём столько же
        // Пауза, чтобы домен gb_clk успел увидеть фронт тоггла до
        // следующей записи: одна его тактовая длиннее нашей в разы.
        S_GBEXT: begin
          if (gb_ext_wait == 0) state <= S_IDLE;
          else gb_ext_wait <= gb_ext_wait - 1'b1;
        end
        S_SCC_A: begin
          if (cen_scc) scc_wait <= scc_wait + 1'b1;
          if (scc_wait >= 4'd2) begin
            scc_cs_n <= 1;
            scc_wr_n <= 1;
            scc_wait <= 0;
            state <= S_SCC_Z;
          end
        end
        S_SCC_Z: begin
          if (cen_scc) scc_wait <= scc_wait + 1'b1;
          if (scc_wait >= 4'd2) state <= S_IDLE;
        end
`endif

`ifdef M4_HAS_ARCADE
        // OKIM6295: wrn держался 0 один такт (negedge словлен), снимаем
        S_OKIM: begin
          okim_wrn <= 1;
          state <= S_IDLE;
        end
`endif

`ifdef M4_HAS_ARCADE
        // K053260: держим cs/wr_n 2 такта cen_k060, затем снимаем
        S_K060_A: begin
          if (cen_k060) scc_wait <= scc_wait + 1'b1;
          if (scc_wait >= 4'd2) begin
            k060_cs <= 0;
            k060_wr_n <= 1;
            scc_wait <= 0;
            state <= S_K060_Z;
          end
        end
        S_K060_Z: begin
          if (cen_k060) scc_wait <= scc_wait + 1'b1;
          if (scc_wait >= 4'd2) state <= S_IDLE;
        end
`endif

        // Байт DPCM в окно NES-RAM: ждём свободного канала записи
        S_NESRAM: begin
          if (!fsm_wr_pending && !fsm_wr_req) begin
            fsm_wr_req <= 1;
            state <= S_IDLE;
          end
        end

        // Записи в MSM6258: ждём освобождения латча (и байтов префетчера)
`ifdef M4_HAS_ARCADE
        S_ADPCM_POLL: begin
          if (!adpcm_wr_pending && !pf_wr) begin
            adpcm_fsm_wr <= 1;
            state <= S_ADPCM_WR;
          end
        end

        S_ADPCM_WR: begin
          adpcm_fsm_wr <= 0;
          state <= S_IDLE;
        end

`endif
        S_AY_WR: begin
          ay_cs_n <= 1;
          ay_wr_n <= 1;
`ifdef M4_HAS_ARCADE
          pcm_cs <= 0;
`endif
          state <= S_IDLE;
        end

        // Ожидание снятия busy: держим чтение статуса
`ifdef M4_HAS_ARCADE
        S_POLL_A, S_POLL_D: begin
          ym_cs_n <= 0;
          ym_wr_n <= 1;
          poll_guard <= poll_guard - 1'b1;
          if (!ym_busy || poll_guard == 0) begin
            ym_a0   <= state == S_POLL_D;
            ym_din  <= state == S_POLL_A ? cur_reg : cur_val;
            ym_cs_n <= 0;
            ym_wr_n <= 0;
            state   <= state == S_POLL_A ? S_WR_A : S_WR_D;
          end
        end

        // Держим строб записи до импульса cen включительно,
        // чтобы jt51 гарантированно защёлкнул busy на cen
        S_WR_A, S_WR_D: begin
          if (cen) begin
            ym_cs_n <= 1;
            ym_wr_n <= 1;
            if (state == S_WR_A) begin
              poll_guard <= 16'hFFFF;
              state <= S_POLL_D;
            end else begin
              state <= S_IDLE;
            end
          end
        end
`endif

        default: state <= S_IDLE;
      endcase
    end
  end

endmodule
