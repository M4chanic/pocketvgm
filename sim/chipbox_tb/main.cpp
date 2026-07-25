// chipbox_tb: интеграционный тест секвенсора chipbox.
// Играет .vgm/.vgz через Wishbone-интерфейс (как это будет делать фирмварь)
// и пишет WAV с аудио-выхода. Проверяет: FIFO, тайминг тиков, busy-протокол.
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>
#include <map>
#include <algorithm>
#include <zlib.h>
#include "Vchipbox.h"
#include "verilated.h"

// Должно совпадать с параметром CLK_HZ при верилировании (-GCLK_HZ)
static const double CLK_HZ = 8000000.0;

static std::vector<uint8_t> read_maybe_gz(const char* path) {
    gzFile f = gzopen(path, "rb");
    if (!f) { fprintf(stderr, "не открыть %s\n", path); exit(1); }
    std::vector<uint8_t> out;
    uint8_t buf[65536];
    int n;
    while ((n = gzread(f, buf, sizeof buf)) > 0) out.insert(out.end(), buf, buf + n);
    gzclose(f);
    return out;
}

static uint32_t rd32(const std::vector<uint8_t>& d, size_t off) {
    return d[off] | d[off+1] << 8 | d[off+2] << 16 | (uint32_t)d[off+3] << 24;
}

struct Tb {
    Vchipbox top;
    uint64_t cycle = 0;
    uint8_t last_toggle = 0;
    std::vector<int16_t> pcm;
    // Модель внешней памяти сэмплов (PSRAM на железе)
    std::vector<uint8_t> mem = std::vector<uint8_t>(8 << 20, 0);
    bool rd_pending = false;
    uint32_t rd_word = 0;
    // гистограмма GBS-фетчей (M4_SIM)
    std::map<uint32_t, uint64_t> fetch_hist;
    uint8_t last_gbs_toggle = 0;

    Tb() {
        top.reset = 1; top.clk = 0;
        top.stb = 0; top.cyc = 0; top.we = 0; top.addr = 0; top.sel = 0xF;
        top.data_write = 0; top.bte = 0; top.cti = 0;
        top.mem_busy = 0; top.mem_rdata = 0; top.mem_rdata_valid = 0;
        top.slot_upd_info = 0;
        for (int i = 0; i < 8; i++) step();
        top.reset = 0;
    }
    void step() {
        // ответ на чтение, выставленное на прошлом такте
        top.mem_rdata_valid = rd_pending;
        if (rd_pending) {
            top.mem_rdata = mem[rd_word * 2] | mem[rd_word * 2 + 1] << 8;
            rd_pending = false;
        }
        top.clk = 0; top.eval();
        top.clk = 1; top.eval();
        if (top.mem_rd) { rd_word = top.mem_addr; rd_pending = true; }
        if (top.mem_wr) {
            if (top.mem_wbe & 1) mem[top.mem_addr * 2] = top.mem_wdata & 0xFF;
            if (top.mem_wbe & 2) mem[top.mem_addr * 2 + 1] = top.mem_wdata >> 8;
        }
        if (top.dbg_gbs_rom_toggle != last_gbs_toggle) {
            last_gbs_toggle = top.dbg_gbs_rom_toggle;
            fetch_hist[top.dbg_gbs_rom_addr]++;
        }
        if (top.chip_sample_toggle != last_toggle) {
            pcm.push_back((int16_t)top.chip_left);
            pcm.push_back((int16_t)top.chip_right);
            last_toggle = top.chip_sample_toggle;
        }
        cycle++;
    }
    // Одна Wishbone-транзакция; возвращает data_read для чтений
    uint32_t wb(uint32_t word_addr, bool write, uint32_t data = 0) {
        top.addr = word_addr; top.we = write; top.data_write = data;
        top.stb = 1; top.cyc = 1;
        int guard = 100;
        do { step(); } while (!top.ack && --guard);
        if (!guard) { fprintf(stderr, "WB: нет ack\n"); exit(1); }
        uint32_t r = top.data_read;
        top.stb = 0; top.cyc = 0;
        step();
        return r;
    }
    uint32_t fifo_used() { return wb(1, false) & 0x1FFF; }
    bool seq_busy() { return (wb(1, false) >> 29) & 1; }
};

static void write_wav_file(const char* out, const std::vector<int16_t>& pcm, uint32_t rate);

// Изолирующий тест: те же регистры APU, но через VGM-путь (FIFO), без CPU
static int apu_selftest(const char* out, double seconds) {
    Tb tb;
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64);
    tb.wb(0xB, true, (uint32_t)(1789773.0 / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    const uint32_t regs[][2] = {{0x15,0x0F},{0x00,0xBF},{0x01,0x00},{0x02,0xFD},{0x03,0x00}};
    for (auto& r : regs) tb.wb(0, true, 0x90000000u | r[0] << 8 | r[1]);
    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();
    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int zc = 0;
    for (size_t i = n/2; i < n; i++)
        if ((tb.pcm[2*i] >= 0) != (tb.pcm[2*(i-1)] >= 0)) zc++;
    int16_t peak = 0;
    for (size_t i = n/2; i < n; i++) peak = std::max(peak, (int16_t)abs(tb.pcm[2*i]));
    fprintf(stderr, "APU-VGM-тест: peak(вторая половина)=%d, zc=%d → %s\n",
            peak, zc, (peak > 2000 && zc > 100) ? "OK" : "FAIL");
    return 0;
}

// Селфтест NSF-режима: 6502 играет пульс с растущей высотой.
// INIT/PLAY лежат в PSRAM (виден как $8000+ через identity-банки),
// стаб — в $5000, векторы — теневые.
static int nsf_selftest(const char* out, double seconds) {
    static const uint8_t prog[] = {
        // INIT @ $8000 (ровно 29 байт + 3 паддинга = 0x20)
        0xA9, 0x0F, 0x8D, 0x15, 0x40,  // LDA #$0F, STA $4015
        0xA9, 0xBF, 0x8D, 0x00, 0x40,  // duty 10, halt, vol 15
        0xA9, 0x00, 0x8D, 0x01, 0x40,  // sweep off
        0xA9, 0xFD, 0x8D, 0x02, 0x40,  // timer lo
        0x8D, 0x00, 0x02,              // STA $0200 (текущий период)
        0xA9, 0x00, 0x8D, 0x03, 0x40,  // timer hi
        0x60,                          // RTS      ($801C)
        0x00, 0x00, 0x00,              // паддинг до $8020
        // PLAY @ $8020: период -1 => высота растёт
        0xAD, 0x00, 0x02,              // LDA $0200
        0x38, 0xE9, 0x01,              // SEC, SBC #1
        0x8D, 0x00, 0x02,              // STA $0200
        0x8D, 0x02, 0x40,              // STA $4002
        0x60,                          // RTS
    };
    static const uint8_t stub[] = {
        0x78,                          // $5000 SEI
        0x20, 0x00, 0x80,              // JSR $8000 (INIT)
        0xAD, 0xF0, 0x5F,              // $5004 LDA $5FF0
        0xF0, 0xFB,                    // BEQ $5004
        0x8D, 0xF0, 0x5F,              // STA $5FF0 (сброс тика)
        0x20, 0x20, 0x80,              // JSR $8020 (PLAY)
        0x4C, 0x04, 0x50,              // JMP $5004
        0x40,                          // $5012 RTI (NMI/IRQ)
    };
    static const uint8_t vecs[6] = {0x12, 0x50, 0x00, 0x50, 0x12, 0x50};

    Tb tb;
    tb.wb(6, true, 0);                 // всё глушим, кроме APU
    tb.wb(0xC, true, 64);
    tb.wb(0xB, true, (uint32_t)(1789773.0 / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0xF, true, (uint32_t)(60.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(8, true, 0x700000);          // программа в NSF-регион PSRAM
    for (size_t i = 0; i < sizeof prog; i += 2)
        tb.wb(9, true, prog[i] | (i + 1 < sizeof prog ? prog[i+1] << 8 : 0));
    for (size_t i = 0; i < sizeof stub; i++) tb.wb(0xD, true, i << 8 | stub[i]);
    for (size_t i = 0; i < 6; i++) tb.wb(0xE, true, i << 8 | vecs[i]);

    tb.wb(2, true, 1);                 // сброс чипов
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 6);                 // nsf_mode | cpu_run

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t s = tb.wb(2, false);
    uint32_t lw = tb.wb(0, false);
    fprintf(stderr, "деб: декодов записи APU: %u, стробов на PHI2: %u, AB=%04x, последняя запись [%02x]=%02x\n",
            s >> 24, (s >> 16) & 0xFF, s & 0xFFFF, (lw >> 16) & 0x1F, lw & 0xFF);

    // канал отладочного чтения PSRAM (0x1F): первый байт программы = 0xA9
    tb.wb(0x1F, true, 0x700000);
    for (int i = 0; i < 64; i++) tb.step();
    uint32_t dbg = tb.wb(0x1F, false);
    if ((dbg & 0x1FF) != 0x1A9) {
        fprintf(stderr, "канал 0x1F: ОЖИДАЛ 0x1A9, получил 0x%03x -> FAIL\n", dbg & 0x1FF);
        return 1;
    }
    fprintf(stderr, "канал 0x1F: чтение PSRAM ok (0xA9)\n");

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);

    size_t n = tb.pcm.size() / 2;
    auto zc = [&](size_t from, size_t to) {
        int c = 0;
        for (size_t i = from + 1; i < to; i++)
            if ((tb.pcm[2*i] >= 0) != (tb.pcm[2*(i-1)] >= 0)) c++;
        return c * 1.0 / (to - from) * rate / 2;
    };
    int16_t peak = 0;
    for (size_t i = 0; i < n; i++) peak = std::max(peak, (int16_t)abs(tb.pcm[2*i]));
    double f0 = zc(n/10, n/4), f1 = zc(3*n/4, n - 1);
    fprintf(stderr, "селфтест NSF: peak=%d, тон в начале ~%.0f Гц, в конце ~%.0f Гц → %s\n",
            peak, f0, f1, (peak > 2000 && f1 > f0 * 1.2) ? "OK" : "FAIL");
    return (peak > 2000 && f1 > f0 * 1.2) ? 0 : 1;
}

// Проигрывание настоящего .nsf через 6502+APU (небанкованный, load >= $8000).
// Схема как в nsf_selftest: программа в PSRAM, стаб в $5000, теневые векторы.
static int nsf_file(const char* path, const char* out, double seconds) {
    std::vector<uint8_t> d = read_maybe_gz(path);
    if (d.size() < 0x81 || memcmp(d.data(), "NESM\x1a", 5)) {
        fprintf(stderr, "не NSF: %s\n", path);
        return 1;
    }
    uint16_t load = d[0x08] | d[0x09] << 8;
    uint16_t init = d[0x0A] | d[0x0B] << 8;
    uint16_t play = d[0x0C] | d[0x0D] << 8;
    bool banked = false;
    for (int i = 0; i < 8; i++) banked |= d[0x70 + i] != 0;
    if (banked || load < 0x8000 || (load & 1)) {
        fprintf(stderr, "поддержан только небанкованный NSF с чётным load >= $8000\n");
        return 1;
    }
    fprintf(stderr, "NSF: load=%04x init=%04x play=%04x len=%zu\n",
            load, init, play, d.size());

    const uint8_t stub[] = {
        0x78,                                          // $5000 SEI
        0xA9, 0x00,                                    // LDA #0 (первая песня)
        0x20, (uint8_t)init, (uint8_t)(init >> 8),     // JSR INIT
        0xAD, 0xF0, 0x5F,                              // $5006 LDA $5FF0
        0xF0, 0xFB,                                    // BEQ $5006
        0x8D, 0xF0, 0x5F,                              // STA $5FF0 (сброс тика)
        0x20, (uint8_t)play, (uint8_t)(play >> 8),     // JSR PLAY
        0x4C, 0x06, 0x50,                              // JMP $5006
        0x40,                                          // $5014 RTI
    };
    static const uint8_t vecs[6] = {0x14, 0x50, 0x00, 0x50, 0x14, 0x50};

    Tb tb;
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64);
    tb.wb(0xB, true, (uint32_t)(1789773.0 / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0xF, true, (uint32_t)(60.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(8, true, 0x700000 + (load - 0x8000));
    for (size_t i = 0x80; i < d.size(); i += 2)
        tb.wb(9, true, d[i] | (i + 1 < d.size() ? d[i + 1] << 8 : 0));
    for (size_t i = 0; i < sizeof stub; i++) tb.wb(0xD, true, i << 8 | stub[i]);
    for (size_t i = 0; i < 6; i++) tb.wb(0xE, true, i << 8 | vecs[i]);

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 6);                 // nsf_mode | cpu_run

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int16_t peak = 0;
    for (size_t i = n / 2; i < n; i++)
        peak = std::max(peak, (int16_t)abs(tb.pcm[2 * i]));
    fprintf(stderr, "nsffile: peak(вторая половина)=%d -> %s\n", peak, out);
    return peak > 500 ? 0 : 1;
}

// Селфтест VRC6: 6502 пишет пульс-канал VRC6 ($9000-$9002), APU молчит.
// Контрол: nsf_mode | cpu_run | vrc6_en (0x86).
static int vrc6_selftest(const char* out, double seconds) {
    static const uint8_t prog[] = {
        // INIT @ $8000: пульс1 VRC6 — duty 4 (7/16), vol 15, период ~$0FD
        0xA9, 0x4F, 0x8D, 0x00, 0x90,  // LDA #$4F, STA $9000
        0xA9, 0xFD, 0x8D, 0x01, 0x90,  // период low
        0xA9, 0x80, 0x8D, 0x02, 0x90,  // enable | период hi 0
        0x60,                          // RTS
    };
    static const uint8_t stub[] = {
        0x78,                          // SEI
        0x20, 0x00, 0x80,              // JSR $8000
        0x4C, 0x04, 0x50,              // $5004: JMP $5004 (PLAY не нужен)
        0x40,                          // $5007 RTI
    };
    static const uint8_t vecs[6] = {0x07, 0x50, 0x00, 0x50, 0x07, 0x50};

    Tb tb;
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64); // канал APU (VRC6 подмешан в него)
    tb.wb(0xB, true, (uint32_t)(1789773.0 / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0xF, true, (uint32_t)(60.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(8, true, 0x700000);
    for (size_t i = 0; i < sizeof prog; i += 2)
        tb.wb(9, true, prog[i] | (i + 1 < sizeof prog ? prog[i+1] << 8 : 0));
    for (size_t i = 0; i < sizeof stub; i++) tb.wb(0xD, true, i << 8 | stub[i]);
    for (size_t i = 0; i < 6; i++) tb.wb(0xE, true, i << 8 | vecs[i]);

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0x86); // nsf_mode | cpu_run | vrc6_en

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int16_t peak = 0;
    int zc = 0;
    for (size_t i = n/2; i < n; i++) {
        peak = std::max(peak, (int16_t)abs(tb.pcm[2*i]));
        if (i > n/2 && (tb.pcm[2*i] >= 0) != (tb.pcm[2*(i-1)] >= 0)) zc++;
    }
    double hz = zc * 1.0 / (n - n/2) * rate / 2;
    // период $0FD при 1.79 МГц: f = clk / (16*(P+1)) ~ 440 Гц
    bool ok = peak > 1000 && hz > 250 && hz < 700;
    fprintf(stderr, "селфтест VRC6: peak=%d, тон ~%.0f Гц → %s\n", peak, hz, ok ? "OK" : "FAIL");
    return ok ? 0 : 1;
}

// Селфтест GBS: SM83 играет пульс на GB APU, PLAY повышает частоту.
// В симуляции gb_clk замедлен до ~1 МГц (клок сима всего 8 МГц).
static int gbs_selftest(const char* out, double seconds) {
    // тело на $00A0: бут-инъекция железа (JP $00A0 с PC=$0000)
    static const uint8_t stub_body[] = {
        0xF3,                    // $00A0 DI
        0x31, 0xFF, 0xDF,        // LD SP,$DFFF
        0x3E, 0x00,              // LD A,0 (песня)
        0xCD, 0x00, 0x04,        // CALL $0400 (INIT)
        0xFA, 0xA0, 0xFE,        // $00A9 LD A,($FEA0)
        0xA7,                    // AND A
        0x28, 0xFA,              // JR Z,-6 -> $00A9
        0xEA, 0xA0, 0xFE,        // LD ($FEA0),A (сброс тика)
        0xCD, 0x20, 0x04,        // CALL $0420 (PLAY)
        0xC3, 0xA9, 0x00,        // JP $00A9
    };
    uint8_t stub[0x100] = {0};
    memcpy(stub + 0xA0, stub_body, sizeof stub_body);
    static const uint8_t prog[] = {
        // INIT @ $0400 (25 байт + паддинг до $0420)
        0x3E, 0x80, 0xE0, 0x26,  // NR52: звук вкл
        0x3E, 0x77, 0xE0, 0x24,  // NR50: громкость
        0x3E, 0xFF, 0xE0, 0x25,  // NR51: каналы в оба выхода
        0x3E, 0x80, 0xE0, 0x11,  // NR11: duty 50%
        0x3E, 0xF0, 0xE0, 0x12,  // NR12: vol 15
        0x3E, 0xD6, 0xE0, 0x13,  // NR13: freq lo
        0x3E, 0x86, 0xE0, 0x14,  // NR14: trigger + freq hi 6
        0xC9,                    // RET
        0, 0, 0,                 // паддинг (29+3=32 = 0x20)
        // PLAY @ $0420: freq lo += 4
        0xFA, 0x00, 0xC0,        // LD A,($C000)
        0xC6, 0x04,              // ADD 4
        0xEA, 0x00, 0xC0,        // LD ($C000),A
        0xE0, 0x13,              // LDH ($13),A
        0xC9,                    // RET
    };

    Tb tb;
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64 << 8);  // только GB
    tb.wb(0x10, true, (uint32_t)(2000000.0 / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0xF, true, (uint32_t)(60.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(8, true, 0x700000 + 0x400);
    for (size_t i = 0; i < sizeof prog; i += 2)
        tb.wb(9, true, prog[i] | (i + 1 < sizeof prog ? prog[i+1] << 8 : 0));
    for (size_t i = 0; i < sizeof stub; i++) tb.wb(0x11, true, i << 8 | stub[i]);

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0xC);  // gbs_mode | cpu_run

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    auto zc = [&](size_t from, size_t to) {
        int c = 0;
        for (size_t i = from + 1; i < to; i++)
            if ((tb.pcm[2*i] >= 0) != (tb.pcm[2*(i-1)] >= 0)) c++;
        return c * 1.0 / (to - from) * rate / 2;
    };
    int16_t peak = 0;
    for (size_t i = n / 4; i < n; i++) peak = std::max(peak, (int16_t)abs(tb.pcm[2*i]));
    double f0 = zc(n/10, n/4), f1 = zc(3*n/4, n - 1);
    fprintf(stderr, "селфтест GBS: peak=%d, тон в начале ~%.0f Гц, в конце ~%.0f Гц → %s\n",
            peak, f0, f1, (peak > 1000 && f1 > f0 * 1.1) ? "OK" : "FAIL");
    return (peak > 1000 && f1 > f0 * 1.1) ? 0 : 1;
}

// Селфтест прерывания GBS: INIT ставит IE=vblank, EI, HALT — и играет тон
// ТОЛЬКО после пробуждения. Если vblank не доставлен, HALT висит вечно и
// звука нет. Векторный обработчик $0040 в стабе = RETI. (Регресс для
// GBDK-рипов, которые ждут vblank в INIT.)
static int gbs_int_selftest(const char* out, double seconds) {
    uint8_t stub[0x100] = {0};
    static const uint8_t stub_body[] = {
        0xF3,                    // $00A0 DI
        0x31, 0xFF, 0xDF,        // LD SP,$DFFF
        0x3E, 0x00,              // LD A,0
        0xCD, 0x00, 0x04,        // CALL $0400 (INIT c HALT)
        0xFA, 0xA0, 0xFE,        // $00A9 LD A,($FEA0)
        0xA7,                    // AND A
        0x28, 0xFA,              // JR Z,-6
        0xEA, 0xA0, 0xFE,        // LD ($FEA0),A
        0xCD, 0x40, 0x04,        // CALL $0440 (PLAY = RET)
        0xC3, 0xA9, 0x00,        // JP $00A9
    };
    memcpy(stub + 0xA0, stub_body, sizeof stub_body);
    stub[0x40] = 0xD9;           // $0040 vblank-вектор: RETI
    static const uint8_t prog[] = {
        // INIT @ $0400: включить vblank, EI, HALT — потом тон
        0x3E, 0x01, 0xE0, 0xFF,  // LD A,1; LDH ($FF),A  -> IE=vblank
        0xFB,                    // EI
        0x76,                    // HALT (ждём vblank)
        0x3E, 0x80, 0xE0, 0x26,  // NR52 on
        0x3E, 0x77, 0xE0, 0x24,  // NR50
        0x3E, 0xFF, 0xE0, 0x25,  // NR51
        0x3E, 0x80, 0xE0, 0x11,  // NR11 duty
        0x3E, 0xF0, 0xE0, 0x12,  // NR12 vol
        0x3E, 0xD6, 0xE0, 0x13,  // NR13 freq lo
        0x3E, 0x86, 0xE0, 0x14,  // NR14 trigger + freq hi
        0xC9,                    // RET
    };
    uint8_t prog_full[0x50] = {0};
    memcpy(prog_full, prog, sizeof prog);
    prog_full[0x40] = 0xC9;      // PLAY @ $0440: RET

    Tb tb;
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64 << 8);   // только GB
    tb.wb(0x10, true, (uint32_t)(2000000.0 / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0xF, true, (uint32_t)(60.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(8, true, 0x700000 + 0x400);
    for (size_t i = 0; i < sizeof prog_full; i += 2)
        tb.wb(9, true, prog_full[i] | (i + 1 < sizeof prog_full ? prog_full[i + 1] << 8 : 0));
    for (size_t i = 0; i < sizeof stub; i++) tb.wb(0x11, true, i << 8 | stub[i]);

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0xC);         // gbs_mode | cpu_run

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int16_t peak = 0;
    for (size_t i = n / 2; i < n; i++) peak = std::max(peak, (int16_t)abs(tb.pcm[2 * i]));
    bool ok = peak > 1000;
    fprintf(stderr, "селфтест GBS-int: peak=%d (тон после HALT) → %s\n", peak, ok ? "OK" : "FAIL");
    return ok ? 0 : 1;
}

// Селфтест SID: 6502 в C64-карте (вся память в PSRAM), пила ~440 Гц,
// PLAY гоняет верхний байт частоты по кольцу через счётчик в RAM $0300.
// Селфтест SCC: грузим синус в волновую таблицу ch1, ставим частоту/
// громкость/keyon и проверяем тон. f = mclock/(32*(freq+1)); freq=0xFD ~ 220 Гц.
static int scc_selftest(const char* out, double seconds) {
    Tb tb;
    // все прочие микс-каналы в ноль, SCC на полную
    tb.wb(6, true, 0);            // mix_gains (ym/ay/pcm/adpcm) = 0
    tb.wb(0xC, true, 0);          // opl/sid/gb/apu = 0
    tb.wb(0x15, true, 0);         // sn/fm = 0
    tb.wb(0x22, true, 64);        // scc_gain = 64
    tb.wb(0x21, true, (uint32_t)(1789772.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(2, true, 1);            // сброс чипов
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0);           // снять сброс
    for (int i = 0; i < 1200; i++) tb.step();  // дождаться снятия chip_reset

    auto scc = [&](int port, int reg, int data) {
        tb.wb(0, true, 0xF0000000u | (port << 16) | (reg << 8) | (data & 0xFF));
    };
    scc(7, 0, 0);                // разблокировка BR2 = 0x3F
    // синус, 32 знаковых 8-битных отсчёта
    for (int i = 0; i < 32; i++) {
        int v = (int)lround(127.0 * sin(2.0 * M_PI * i / 32.0));
        scc(0, i, v & 0xFF);     // порт 0: waveform ch1 (0x00-0x1F)
    }
    scc(1, 0, 0xFD);             // порт 1: freq ch1 lo
    scc(1, 1, 0x00);             // freq ch1 hi
    scc(2, 0, 0x0F);             // порт 2: volume ch1 = max
    scc(3, 0, 0x01);             // порт 3: keyon, ch1

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int16_t peak = 0;
    int zc = 0;
    for (size_t i = n / 2; i < n; i++) {
        peak = std::max(peak, (int16_t)abs(tb.pcm[2 * i]));
        if (i > n / 2 && (tb.pcm[2 * i] >= 0) != (tb.pcm[2 * (i - 1)] >= 0)) zc++;
    }
    double hz = zc * 1.0 / (n - n / 2) * rate / 2;
    // ожидаемая высота 1789772/(32*(0xFD+1)) ~ 220 Гц
    bool ok = peak > 1000 && hz > 185 && hz < 260;
    fprintf(stderr, "селфтест SCC: peak=%d, тон ~%.0f Гц (ждём ~220) → %s\n", peak, hz, ok ? "OK" : "FAIL");
    return ok ? 0 : 1;
}

// Селфтест OKIM6295: кладём таблицу фраз + ADPCM-данные (треугольник) в
// PSRAM-модель по OKIM_BASE (байт 0x100000), запускаем фразу 0 двухбайтовой
// командой и проверяем, что чип фетчит ROM и выдаёт звук.
static int okim_selftest(const char* out, double seconds) {
    Tb tb;
    const uint32_t OKIM_BASE = 0x100000;
    // фраза 0: заголовок в байтах 0..7, данные 0x400.. (длинные, на всю
    // секунду воспроизведения: OKIM6295 не зациклен)
    uint32_t start = 0x400, stop = 0x4000;
    // jt6295 читает фразу как start[17:0] в байтах 0-2, stop в байтах 3-5
    tb.mem[OKIM_BASE + 0] = (start >> 16) & 3;
    tb.mem[OKIM_BASE + 1] = (start >> 8) & 0xFF;
    tb.mem[OKIM_BASE + 2] = start & 0xFF;
    tb.mem[OKIM_BASE + 3] = (stop >> 16) & 3;
    tb.mem[OKIM_BASE + 4] = (stop >> 8) & 0xFF;
    tb.mem[OKIM_BASE + 5] = stop & 0xFF;
    // ADPCM: 4 байта макс.+дельт (подъём), 4 байта макс.-дельт (спуск)
    for (uint32_t a = start; a <= stop; a++)
        tb.mem[OKIM_BASE + a] = ((a >> 2) & 1) ? 0xFF : 0x77;

    tb.wb(6, true, 0);            // ym/ay/pcm/adpcm = 0
    tb.wb(0xC, true, 0);          // opl/sid/gb/apu = 0
    tb.wb(0x15, true, 0);         // sn/fm = 0
    tb.wb(0x22, true, 0);         // scc = 0
    tb.wb(0x24, true, (1u << 8) | 64);  // okim: ss=1, gain=64
    tb.wb(0x23, true, (uint32_t)(1000000.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0);
    for (int i = 0; i < 1200; i++) tb.step();

    auto okim = [&](int data) {
        tb.wb(0, true, 0xF1000000u | (data & 0xFF));  // OP_EXT|EXT_OKIM
    };
    okim(0x80);                  // старт фразы 0 (бит7=1, phrase=0)
    okim(0x10);                  // канал 0 (маска 0001), аттенюация 0
    for (int i = 0; i < 512; i++) tb.step();

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int16_t peak = 0;
    for (size_t i = n / 4; i < n; i++)  // вторая половина: устойчивое воспр.
        peak = std::max(peak, (int16_t)abs(tb.pcm[2 * i]));
    bool ok = peak > 1000;
    fprintf(stderr, "селфтест OKIM6295: peak=%d (ADPCM из PSRAM) → %s\n", peak, ok ? "OK" : "FAIL");
    return ok ? 0 : 1;
}

// Селфтест K053260: кладём PCM-синус (8-бит знак) в PSRAM по K060_BASE,
// настраиваем канал 0 (start/length/pitch/vol/pan), даём keyon по фронту
// и проверяем звук. Один из 4 каналов с общим ROM через round-robin фетч.
static int k060_selftest(const char* out, double seconds) {
    Tb tb;
    const uint32_t K060_BASE = 0x200000;
    uint32_t start = 0x400, length = 64;
    for (uint32_t i = 0; i < length; i++)
        tb.mem[K060_BASE + start + i] = (int8_t)lround(100.0 * sin(2.0 * M_PI * i / length)) & 0xFF;

    tb.wb(6, true, 0);
    tb.wb(0xC, true, 0);
    tb.wb(0x15, true, 0);
    tb.wb(0x22, true, 0);
    tb.wb(0x24, true, 0);        // okim off
    tb.wb(0x26, true, 64);       // k060_gain
    tb.wb(0x25, true, (uint32_t)(3579545.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0);
    for (int i = 0; i < 1200; i++) tb.step();

    auto k060 = [&](int reg, int data) {
        tb.wb(0, true, 0xF2000000u | ((reg & 0x3F) << 8) | (data & 0xFF));
        for (int i = 0; i < 40; i++) tb.step();  // дать строб cs/wr_n завершиться
    };
    int pitch = 0xF00;
    k060(0x28, 0x00);            // keyon все выкл (сброс дефолта 0xF)
    k060(0x08, pitch & 0xFF);    // ch0 pitch lo
    k060(0x09, (pitch >> 8) & 0x0F);
    k060(0x0A, length & 0xFF);   // length lo
    k060(0x0B, (length >> 8) & 0xFF);
    k060(0x0C, start & 0xFF);    // start lo
    k060(0x0D, (start >> 8) & 0xFF);
    k060(0x0E, (start >> 16) & 0x1F);
    k060(0x0F, 0x40);            // volume
    k060(0x2A, 0x01);            // loop ch0, adpcm off
    k060(0x2C, 0x04);            // ch0 pan = центр
    k060(0x2F, 0x02);            // mode[1]=1: разрешить выход (иначе fade)
    k060(0x28, 0x01);            // keyon ch0 -> фронт, старт

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int16_t peak = 0;
    for (size_t i = n / 4; i < n; i++)
        peak = std::max(peak, (int16_t)abs(tb.pcm[2 * i]));
    bool ok = peak > 500;
    fprintf(stderr, "селфтест K053260: peak=%d (PCM из PSRAM, канал 0) → %s\n", peak, ok ? "OK" : "FAIL");
    return ok ? 0 : 1;
}

static int sid_selftest(const char* out, double seconds) {
    static const uint8_t stub[] = {
        0x78,                    // $0334 SEI
        0xA9, 0x00,              // LDA #0 (песня)
        0x20, 0x00, 0x10,        // JSR $1000 (INIT)
        0xAD, 0xF0, 0xD7,        // $033A LDA $D7F0
        0xF0, 0xFB,              // BEQ $033A
        0x8D, 0xF0, 0xD7,        // STA $D7F0 (сброс тика)
        0x20, 0x30, 0x10,        // JSR $1030 (PLAY)
        0x4C, 0x3A, 0x03,        // JMP $033A
        0x40,                    // $0348 RTI
    };
    static const uint8_t prog[] = {
        // INIT @ $1000
        0xA9, 0x45, 0x8D, 0x00, 0xD4,  // freq lo
        0xA9, 0x1D, 0x8D, 0x01, 0xD4,  // freq hi (~440 Гц)
        0xA9, 0x00, 0x8D, 0x05, 0xD4,  // attack/decay
        0xA9, 0xF0, 0x8D, 0x06, 0xD4,  // sustain 15
        0xA9, 0x0F, 0x8D, 0x18, 0xD4,  // громкость 15
        0xA9, 0x21, 0x8D, 0x04, 0xD4,  // пила + gate
        0x60,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // до $1030
        // PLAY @ $1030
        0xAD, 0x00, 0x03,        // LDA $0300
        0x18, 0x69, 0x01,        // CLC, ADC 1
        0x8D, 0x00, 0x03,        // STA $0300
        0x29, 0x0F,              // AND #$0F
        0x18, 0x69, 0x1D,        // CLC, ADC #$1D
        0x8D, 0x01, 0xD4,        // STA $D401
        0x60,
    };
    static const uint8_t vecs[6] = {0x48, 0x03, 0x34, 0x03, 0x48, 0x03};

    Tb tb;
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64 << 16);  // только SID
    // в симе clk всего 8 МГц: конвейеру SID нужно >=14 тактов на цикл
    // ce_1m — замедляем чип вчетверо (тон тоже /4, на железе полный клок)
    tb.wb(0x12, true, (uint32_t)(985248.0 / 4 / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0xF, true, (uint32_t)(50.0 / CLK_HZ * 4294967296.0 + 0.5));

    auto upload = [&](uint32_t addr, const uint8_t* p, size_t n) {
        tb.wb(8, true, 0x700000 + addr);
        for (size_t i = 0; i < n; i += 2)
            tb.wb(9, true, p[i] | (i + 1 < n ? p[i+1] << 8 : 0));
    };
    upload(0x0334, stub, sizeof stub);
    upload(0x1000, prog, sizeof prog);
    upload(0xFFFA, vecs, 6);

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0x14);  // sid_mode | cpu_run

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int zc = 0;
    int16_t peak = 0;
    for (size_t i = n/2 + 1; i < n; i++) {
        if ((tb.pcm[2*i] >= 0) != (tb.pcm[2*(i-1)] >= 0)) zc++;
        peak = std::max(peak, (int16_t)abs(tb.pcm[2*i]));
    }
    double hz = zc * 1.0 / (n - n/2) * rate / 2;
    fprintf(stderr, "селфтест SID: peak=%d, средний тон ~%.0f Гц → %s\n",
            peak, hz, (peak > 1000 && hz > 50) ? "OK" : "FAIL");
    return (peak > 1000 && hz > 50) ? 0 : 1;
}

// Селфтест паузы (контрол бит 5): тон SN76489, пауза морозит выход
// (константа, размах ~0), снятие паузы оживляет.
static int pause_selftest(const char* out) {
    Tb tb;
    // SN на 3.58 МГц, в миксе только он
    tb.wb(0x17, true, (uint32_t)(3579545.0 / CLK_HZ * 4294967296.0));
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 0);
    tb.wb(0x15, true, 32u << 8);
    tb.wb(2, true, 1);
    for (int i = 0; i < 4096; i++) tb.step();

    // тон ~440 Гц на канале 0, громкость максимум, затем длинные WAIT
    tb.wb(0, true, 0xE0000000u | 0x8E);
    tb.wb(0, true, 0xE0000000u | 0x0F);
    tb.wb(0, true, 0xE0000000u | 0x90);
    for (int i = 0; i < 8; i++) tb.wb(0, true, 0x80000000u | 44100);

    auto run = [&](double sec) { uint64_t to = tb.cycle + (uint64_t)(sec * CLK_HZ); while (tb.cycle < to) tb.step(); };
    auto spread = [&](size_t from) {
        int16_t lo = 32767, hi = -32768;
        for (size_t i = from; i < tb.pcm.size() / 2; i++) {
            lo = std::min(lo, tb.pcm[2*i]);
            hi = std::max(hi, tb.pcm[2*i]);
        }
        return (int)hi - lo;
    };

    run(0.4);
    size_t m1 = tb.pcm.size() / 2;
    int s_play = spread(m1 / 2);
    uint32_t vu_play = tb.wb(0x1A, false) & 0xFFFF; // и очистка пиков

    tb.wb(2, true, 0x20);      // пауза
    run(0.15);                 // дать фронту дозвучать
    size_t m2 = tb.pcm.size() / 2;
    run(0.25);
    int s_pause = spread(m2);

    tb.wb(2, true, 0);         // снятие паузы
    run(0.05);
    size_t m3 = tb.pcm.size() / 2;
    run(0.3);
    int s_resume = spread(m3);

    write_wav_file(out, tb.pcm, (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5));
    tb.wb(0x1A, false); // это чтение очищает пики...
    uint32_t vu_clr = tb.wb(0x1A, false) & 0xFFFF; // ...второе сразу — почти ноль
    bool ok = s_play > 500 && s_pause <= 8 && s_resume > 500
              && vu_play > 500 && vu_clr < 64;
    fprintf(stderr, "селфтест паузы: размах игра=%d, пауза=%d, продолжение=%d, VU=%u->%u → %s\n",
            s_play, s_pause, s_resume, vu_play, vu_clr, ok ? "OK" : "FAIL");
    return ok ? 0 : 1;
}

// Селфтест софт-сброса: тон A + горы WAIT в FIFO, софт-сброс, тон B —
// звучать должен B сразу (баг: wr_ptr не чистился, кольцо переигрывалось)
static int reset_selftest(const char* out) {
    Tb tb;
    auto setup = [&]() {
        tb.wb(0x17, true, (uint32_t)(3579545.0 / CLK_HZ * 4294967296.0));
        tb.wb(6, true, 0);
        tb.wb(0xC, true, 0);
        tb.wb(0x15, true, 32u << 8);
    };
    auto run = [&](double sec) { uint64_t to = tb.cycle + (uint64_t)(sec * CLK_HZ); while (tb.cycle < to) tb.step(); };
    auto tone_hz = [&](size_t from) {
        // средний тон по нулям с вычетом DC
        long long sum = 0; size_t n = tb.pcm.size() / 2;
        for (size_t i = from; i < n; i++) sum += tb.pcm[2*i];
        int16_t mid = (int16_t)(sum / (long long)(n - from));
        int zc = 0;
        for (size_t i = from + 1; i < n; i++)
            if ((tb.pcm[2*i] >= mid) != (tb.pcm[2*(i-1)] >= mid)) zc++;
        double rate = (double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ);
        return zc * 0.5 / (n - from) * rate;
    };

    setup();
    tb.wb(2, true, 1);
    for (int i = 0; i < 4096; i++) tb.step();
    // тон A ~440 Гц (n=254) + 8 секунд WAIT'ов в FIFO
    tb.wb(0, true, 0xE0000000u | 0x8E);
    tb.wb(0, true, 0xE0000000u | 0x0F);
    tb.wb(0, true, 0xE0000000u | 0x90);
    for (int i = 0; i < 8; i++) tb.wb(0, true, 0x80000000u | 44100);
    run(0.3);
    double f_a = tone_hz(tb.pcm.size() / 4);

    // «переключение трека»: софт-сброс, настройка заново, тон B ~880 Гц (n=127)
    tb.wb(2, true, 1);
    setup();
    for (int i = 0; i < 4096; i++) tb.step();
    size_t mark = tb.pcm.size() / 2;
    tb.wb(0, true, 0xE0000000u | 0x8F);
    tb.wb(0, true, 0xE0000000u | 0x07);
    tb.wb(0, true, 0xE0000000u | 0x90);
    for (int i = 0; i < 8; i++) tb.wb(0, true, 0x80000000u | 44100);
    run(0.1); // пропустить фронт
    mark = tb.pcm.size() / 2;
    run(0.3);
    double f_b = tone_hz(mark);

    write_wav_file(out, tb.pcm, (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5));
    bool ok = f_a > 350 && f_a < 550 && f_b > 700 && f_b < 1100;
    fprintf(stderr, "селфтест сброса: тон до ~%.0f Гц, после ~%.0f Гц → %s\n",
            f_a, f_b, ok ? "OK" : "FAIL (после сброса должен звучать новый тон)");
    return ok ? 0 : 1;
}

// Селфтест перемотки (контрол бит 6): 8 с WAIT'ов при ff должны
// съесться за ~1 с; без ff секвенсор остался бы занят
static int ff_selftest() {
    Tb tb;
    tb.wb(0x17, true, (uint32_t)(3579545.0 / CLK_HZ * 4294967296.0));
    tb.wb(6, true, 0); tb.wb(0xC, true, 0); tb.wb(0x15, true, 32u << 8);
    tb.wb(2, true, 1);
    for (int i = 0; i < 4096; i++) tb.step();
    tb.wb(0, true, 0xE0000000u | 0x8E);
    tb.wb(0, true, 0xE0000000u | 0x0F);
    tb.wb(0, true, 0xE0000000u | 0x90);
    for (int i = 0; i < 8; i++) tb.wb(0, true, 0x80000000u | 44100);
    tb.wb(2, true, 0x40); // fast-forward
    uint64_t to = tb.cycle + (uint64_t)(1.5 * CLK_HZ);
    while (tb.cycle < to) tb.step();
    bool drained = !tb.seq_busy();
    tb.wb(2, true, 0);
    fprintf(stderr, "селфтест ff: 8 c WAIT'ов за 1.5 c %s → %s\n",
            drained ? "съедены" : "НЕ съедены", drained ? "OK" : "FAIL");
    return drained ? 0 : 1;
}

// Прогон НАСТОЯЩЕГО GBS-файла (как фирмварь): данные в PSRAM, стаб,
// play-тик из заголовка; печатает t/w/f-счётчики и пишет WAV
static int gbs_file(const char* path, const char* out, double seconds) {
    FILE* f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "не открыть %s\n", path); return 1; }
    std::vector<uint8_t> d;
    uint8_t buf[65536];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0) d.insert(d.end(), buf, buf + n);
    fclose(f);
    if (d.size() < 0x70 || memcmp(d.data(), "GBS", 3)) { fprintf(stderr, "не GBS\n"); return 1; }

    uint8_t song = d[0x05] ? d[0x05] - 1 : 0;
    uint16_t load = d[0x06] | d[0x07] << 8;
    uint16_t init = d[0x08] | d[0x09] << 8;
    uint16_t play = d[0x0A] | d[0x0B] << 8;
    uint16_t sp = d[0x0C] | d[0x0D] << 8;
    uint8_t tma = d[0x0E], tac = d[0x0F];
    fprintf(stderr, "GBS: load=%04x init=%04x play=%04x sp=%04x tma=%02x tac=%02x песня %d\n",
            load, init, play, sp, tma, tac, song + 1);

    Tb tb;
    // данные линейно от load
    tb.wb(8, true, 0x700000 + load);
    for (size_t i = 0x70; i < d.size(); i += 2)
        tb.wb(9, true, d[i] | (i + 1 < d.size() ? d[i+1] << 8 : 0));

    double play_hz = 59.73;
    if (tac & 4) {
        double base = (tac & 3) == 0 ? 4096.0 : (tac & 3) == 1 ? 262144.0
                     : (tac & 3) == 2 ? 65536.0 : 16384.0;
        play_hz = base / (256 - tma);
    }
    tb.wb(0xF, true, (uint32_t)(play_hz / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0x10, true, (uint32_t)(2000000.0 / CLK_HZ * 4294967296.0 + 0.5)); // gb_clk как в селфтесте (~2 МГц, высота ÷2.1)
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64u << 8);

    // стаб как в фирмвари: RST/IRQ-трамплины JP LOAD+n ($00-$60),
    // тело на $00A0 (бут-инъекция железа приводит PC туда с $0000)
    std::vector<uint8_t> stub(0x100, 0x00);
    for (int v = 0; v <= 0x60; v += 8) {
        uint16_t tgt = load + v;
        stub[v] = 0xC3;
        stub[v + 1] = (uint8_t)tgt;
        stub[v + 2] = (uint8_t)(tgt >> 8);
    }
    size_t o = 0xA0;
    stub[o++] = 0xF3;
    stub[o++] = 0x31; stub[o++] = (uint8_t)sp; stub[o++] = (uint8_t)(sp >> 8);
    stub[o++] = 0x3E; stub[o++] = song;
    stub[o++] = 0xCD; stub[o++] = (uint8_t)init; stub[o++] = (uint8_t)(init >> 8);
    uint16_t loop_at = (uint16_t)o;
    stub[o++] = 0xFA; stub[o++] = 0xA0; stub[o++] = 0xFE;
    stub[o++] = 0xA7;
    stub[o++] = 0x28; stub[o++] = 0xFA;
    stub[o++] = 0xEA; stub[o++] = 0xA0; stub[o++] = 0xFE;
    stub[o++] = 0xCD; stub[o++] = (uint8_t)play; stub[o++] = (uint8_t)(play >> 8);
    stub[o++] = 0xC3; stub[o++] = (uint8_t)loop_at; stub[o++] = (uint8_t)(loop_at >> 8);
    for (size_t i = 0; i < stub.size(); i++) tb.wb(0x11, true, i << 8 | stub[i]);

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0xC);

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t tw = tb.wb(0x1E, false);
    uint32_t ff = tb.wb(0x1D, false);
    uint32_t gb = tb.wb(0x1C, false);
    fprintf(stderr, "t=%04x w=%04x f=%04x g=%04x\n",
            tw >> 16, tw & 0xFFFF, ff >> 16, gb >> 16);
    // гистограмма горячих адресов фетчей (собрана в цикле выше)
    {
        std::vector<std::pair<uint64_t,uint32_t>> hot;
        for (auto& kv : tb.fetch_hist) hot.push_back({kv.second, kv.first});
        std::sort(hot.rbegin(), hot.rend());
        fprintf(stderr, "горячие фетчи (ROM-адрес: раз):\n");
        for (size_t i = 0; i < hot.size() && i < 12; i++)
            fprintf(stderr, "  %06x: %llu\n", hot[i].second,
                    (unsigned long long)hot[i].first);
    }

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t np = tb.pcm.size() / 2;
    int16_t peak = 0;
    for (size_t i = np/2; i < np; i++) peak = std::max(peak, (int16_t)abs(tb.pcm[2*i]));
    fprintf(stderr, "peak(вторая половина)=%d -> %s\n", peak, out);
    return 0;
}

// Прогон НАСТОЯЩЕГО PSID-файла (как фирмварь): образ 64К в PSRAM,
// стаб @$0334, векторы, темп из speed-маски; WAV на выходе
static int sid_file(const char* path, const char* out, double seconds) {
    FILE* f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "не открыть %s\n", path); return 1; }
    std::vector<uint8_t> d;
    uint8_t buf[65536];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0) d.insert(d.end(), buf, buf + n);
    fclose(f);
    if (d.size() < 0x76 || memcmp(d.data(), "PSID", 4)) { fprintf(stderr, "не PSID\n"); return 1; }
    auto be16 = [&](size_t o) { return (uint16_t)(d[o] << 8 | d[o+1]); };
    uint16_t data_off = be16(0x06);
    uint16_t load = be16(0x08);
    uint16_t init = be16(0x0A);
    uint16_t play = be16(0x0C);
    uint32_t speed = d[0x12] << 24 | d[0x13] << 16 | d[0x14] << 8 | d[0x15];
    const uint8_t* body = d.data() + data_off;
    size_t blen = d.size() - data_off;
    if (load == 0) { load = body[0] | body[1] << 8; body += 2; blen -= 2; }
    fprintf(stderr, "PSID: load=%04x init=%04x play=%04x speed=%08x len=%zu\n",
            load, init, play, speed, blen);

    Tb tb;
    // SID-клок PAL с учётом замедления x4 (конвейеру sid_top нужно >=14 clk)
    tb.wb(0x12, true, (uint32_t)(985248.0 / 4 / CLK_HZ * 4294967296.0));
    tb.wb(0x13, true, 0); // 6581
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64u << 16);

    // чистый образ: нули + данные + стаб + векторы
    tb.wb(8, true, 0x700000);
    for (int i = 0; i < 0x8000; i++) tb.wb(9, true, 0);
    tb.wb(8, true, 0x700000 + load);
    for (size_t i = 0; i < blen; i += 2)
        tb.wb(9, true, body[i] | (i + 1 < blen ? body[i+1] << 8 : 0));

    std::vector<uint8_t> stub;
    stub.push_back(0x78);
    stub.insert(stub.end(), {0xA9, 0x00});
    stub.insert(stub.end(), {0x20, (uint8_t)init, (uint8_t)(init >> 8)});
    uint16_t loop_at = 0x0334 + (uint16_t)stub.size();
    stub.insert(stub.end(), {0xAD, 0xF0, 0xD7});
    stub.insert(stub.end(), {0xF0, 0xFB});
    stub.insert(stub.end(), {0x8D, 0xF0, 0xD7});
    stub.insert(stub.end(), {0x20, (uint8_t)play, (uint8_t)(play >> 8)});
    stub.insert(stub.end(), {0x4C, (uint8_t)loop_at, (uint8_t)(loop_at >> 8)});
    uint16_t rti_at = 0x0334 + (uint16_t)stub.size();
    stub.push_back(0x40);
    tb.wb(8, true, 0x700000 + 0x334);
    for (size_t i = 0; i < stub.size(); i += 2)
        tb.wb(9, true, stub[i] | (i + 1 < stub.size() ? stub[i+1] << 8 : 0));
    uint8_t vecs[6] = {(uint8_t)rti_at, (uint8_t)(rti_at >> 8), 0x34, 0x03,
                       (uint8_t)rti_at, (uint8_t)(rti_at >> 8)};
    tb.wb(8, true, 0x700000 + 0xFFFA);
    for (int i = 0; i < 6; i += 2) tb.wb(9, true, vecs[i] | vecs[i+1] << 8);

    double hz = (speed & 1) ? 60.0 : 50.12;
    tb.wb(0xF, true, (uint32_t)(hz / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0x14);

    uint64_t cycles = (uint64_t)(seconds * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    uint32_t pw = tb.wb(0x1B, false);
    fprintf(stderr, "p=%04x w=%04x\n", pw >> 16, pw & 0xFFFF);
    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t np = tb.pcm.size() / 2;
    int16_t peak = 0;
    int zc = 0;
    for (size_t i = np/2; i < np; i++) {
        peak = std::max(peak, (int16_t)abs(tb.pcm[2*i]));
        if ((tb.pcm[2*i] >= 0) != (tb.pcm[2*(i-1)] >= 0)) zc++;
    }
    fprintf(stderr, "peak=%d, zc(вторая половина)=%d (~%.0f Гц) -> %s\n",
            peak, zc, zc * 0.5 / (np - np/2) * rate, out);
    return 0;
}

// Проигрывание готового потока команд (mid2cmds и пр.): u32 LE слова
static int play_cmds(const char* path, const char* out) {
    FILE* f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "не открыть %s\n", path); return 1; }
    std::vector<uint32_t> cmds;
    uint32_t w;
    while (fread(&w, 4, 1, f) == 1) cmds.push_back(w);
    fclose(f);
    fprintf(stderr, "команд: %zu\n", cmds.size());

    Tb tb;
    tb.wb(6, true, 0);
    tb.wb(0xC, true, 64u << 24);  // только OPL3
    tb.wb(2, true, 1);
    for (int i = 0; i < 4096; i++) tb.step();

    size_t fed = 0;
    while (fed < cmds.size()) {
        uint32_t used = tb.fifo_used();
        if (used < 1536) {
            size_t batch = std::min(cmds.size() - fed, (size_t)(2000 - used));
            for (size_t i = 0; i < batch; i++) tb.wb(0, true, cmds[fed++]);
        } else {
            for (int i = 0; i < 20000; i++) tb.step();
        }
    }
    while (tb.seq_busy()) for (int i = 0; i < 20000; i++) tb.step();
    for (int i = 0; i < 400000; i++) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    size_t n = tb.pcm.size() / 2;
    int16_t peak = 0;
    for (size_t i = 0; i < n; i++) peak = std::max(peak, (int16_t)abs(tb.pcm[2*i]));
    fprintf(stderr, "готово: %zu сэмплов @ %u Гц, peak=%d -> %s\n", n, rate, peak, out);
    return 0;
}

int main(int argc, char** argv) {
    Verilated::commandArgs(argc, argv);
    const char* in = nullptr; const char* out = "out.wav";
    double max_seconds = 0;
    bool selftest = false;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-o") && i + 1 < argc) out = argv[++i];
        else if (!strcmp(argv[i], "-t") && i + 1 < argc) max_seconds = atof(argv[++i]);
        else if (!strcmp(argv[i], "--nsf-selftest")) selftest = true;
        else if (!strcmp(argv[i], "--apu-selftest")) { return apu_selftest("apu_st.wav", 1.0); }
        else if (!strcmp(argv[i], "--gbs-selftest")) { return gbs_selftest(out, 2.0); }
        else if (!strcmp(argv[i], "--gbs-int-selftest")) { return gbs_int_selftest(out, 2.0); }
        else if (!strcmp(argv[i], "--sid-selftest")) { return sid_selftest(out, 2.0); }
        else if (!strcmp(argv[i], "--cmds") && i + 1 < argc) { return play_cmds(argv[i+1], out); }
        else if (!strcmp(argv[i], "--pause-selftest")) { return pause_selftest(out); }
        else if (!strcmp(argv[i], "--reset-selftest")) { return reset_selftest(out); }
        else if (!strcmp(argv[i], "--ff-selftest")) { return ff_selftest(); }
        else if (!strcmp(argv[i], "--vrc6-selftest")) { return vrc6_selftest(out, 1.0); }
        else if (!strcmp(argv[i], "--scc-selftest")) { return scc_selftest(out, 1.0); }
        else if (!strcmp(argv[i], "--okim-selftest")) { return okim_selftest(out, 1.0); }
        else if (!strcmp(argv[i], "--k060-selftest")) { return k060_selftest(out, 1.0); }
        else if (!strcmp(argv[i], "--gbsfile") && i + 1 < argc) { return gbs_file(argv[++i], out, max_seconds > 0 ? max_seconds : 4.0); }
        else if (!strcmp(argv[i], "--nsffile") && i + 1 < argc) { return nsf_file(argv[++i], out, max_seconds > 0 ? max_seconds : 4.0); }
        else if (!strcmp(argv[i], "--sidfile") && i + 1 < argc) { return sid_file(argv[++i], out, max_seconds > 0 ? max_seconds : 4.0); }
        else in = argv[i];
    }
    if (selftest) return nsf_selftest(out, max_seconds > 0 ? max_seconds : 2.0);
    if (!in) { fprintf(stderr, "usage: chipbox_tb <file.vgm|.vgz> [-o out.wav] [-t sec] | --nsf-selftest\n"); return 1; }

    std::vector<uint8_t> d = read_maybe_gz(in);
    if (d.size() < 0x40 || memcmp(d.data(), "Vgm ", 4)) { fprintf(stderr, "не VGM\n"); return 1; }
    uint32_t version = rd32(d, 0x08);
    size_t data_off = (version >= 0x150) ? 0x34 + rd32(d, 0x34) : 0x40;
    size_t hdr_end = std::min(data_off, (size_t)0x100);
    uint32_t ym_clk = rd32(d, 0x30) & 0x3FFFFFFF;
    uint32_t ay_clk = hdr_end >= 0x78 ? rd32(d, 0x74) & 0x3FFFFFFF : 0;
    uint32_t pcm_clk = hdr_end >= 0x3C ? rd32(d, 0x38) & 0x3FFFFFFF : 0;
    uint32_t adpcm_clk = hdr_end >= 0x94 ? rd32(d, 0x90) & 0x3FFFFFFF : 0;
    uint8_t adpcm_flags = hdr_end >= 0x95 ? d[0x94] : 0;
    uint32_t nes_clk = hdr_end >= 0x88 ? rd32(d, 0x84) & 0x3FFFFFFF : 0;
    uint32_t fm_clk = rd32(d, 0x2C) & 0x3FFFFFFF;
    uint32_t sn_clk = rd32(d, 0x0C) & 0x3FFFFFFF;
    size_t pos = data_off;
    // OPL-семейство: YM3812 (0x50), YM3526 (0x54), YMF262 (0x5C).
    // OPL2-файлы задают 3.58 МГц — ядро тактуется x4 (как в фирмвари).
    uint32_t ym3812_clk = hdr_end >= 0x54 ? rd32(d, 0x50) & 0x3FFFFFFF : 0;
    uint32_t ym3526_clk = hdr_end >= 0x58 ? rd32(d, 0x54) & 0x3FFFFFFF : 0;
    uint32_t ymf262_clk = hdr_end >= 0x60 ? rd32(d, 0x5C) & 0x3FFFFFFF : 0;
    uint32_t opl_clk = ymf262_clk ? ymf262_clk
                     : ym3812_clk ? ym3812_clk * 4
                     : ym3526_clk ? ym3526_clk * 4 : 0;
    uint32_t scc_clk  = hdr_end >= 0xA0 ? rd32(d, 0x9C) & 0x3FFFFFFF : 0;
    uint32_t k060_clk = hdr_end >= 0xB0 ? rd32(d, 0xAC) & 0x3FFFFFFF : 0;

    if (!ym_clk && !ay_clk && !pcm_clk && !adpcm_clk && !nes_clk && !fm_clk && !sn_clk && !opl_clk
        && !scc_clk && !k060_clk) { fprintf(stderr, "в файле нет поддержанных чипов\n"); return 1; }

    // VGM → командные слова chipbox (это же будет делать фирмварь)
    // + отдельно собираем data-блоки SegaPCM ROM (тип 0x80)
    std::vector<uint32_t> cmds;
    struct RomBlock { uint32_t start; std::vector<uint8_t> bytes; };
    std::vector<RomBlock> rom_blocks;
    std::vector<uint8_t> dac_bank;
    uint32_t dac_ptr = 0;
    // Банк ADPCM-потоков (data-блоки типа 0x04, конкатенация) для MSM6258
    const uint32_t ADPCM_BASE = 0x400000;
    std::vector<uint8_t> adpcm_bank;
    struct StrBlock { uint32_t off, len; };
    std::vector<StrBlock> adpcm_blocks;
    uint64_t total_ticks = 0;
    size_t pcm_masked = 0, stream_warn = 0;
    auto wait_ticks = [&](uint32_t n) { if (n) { cmds.push_back(0x80000000u | n); total_ticks += n; } };
    bool run = true;
    while (run && pos < d.size()) {
        uint8_t cmd = d[pos++];
        if (cmd == 0x54) { cmds.push_back(0x10000000u | d[pos] << 8 | d[pos+1]); pos += 2; }
        else if (cmd == 0x52) { cmds.push_back(0xD0000000u | d[pos] << 8 | d[pos+1]); pos += 2; }
        else if (cmd == 0x53) { cmds.push_back(0xD0000000u | 0x10000u | d[pos] << 8 | d[pos+1]); pos += 2; }
        else if (cmd == 0x4F || cmd == 0x50) { cmds.push_back(0xE0000000u | d[pos]); pos += 1; }
        // OPL-семейство на OPL3: YM3812/YM3526/YMF262 п0 — банк 0, YMF262 п1 — банк 1
        else if (cmd == 0x5A || cmd == 0x5B || cmd == 0x5E) {
            cmds.push_back(0xC0000000u | d[pos] << 8 | d[pos+1]); pos += 2;
        }
        else if (cmd == 0x5F) {
            cmds.push_back(0xC0000000u | 0x10000u | d[pos] << 8 | d[pos+1]); pos += 2;
        }
        // SCC (K051649): 0xD2 порт рег знач -> OP_EXT|EXT_SCC
        else if (cmd == 0xD2) {
            cmds.push_back(0xF0000000u | d[pos] << 16 | d[pos+1] << 8 | d[pos+2]); pos += 3;
        }
        // K053260: 0xBA рег знач -> OP_EXT|EXT_K060
        else if (cmd == 0xBA) {
            cmds.push_back(0xF2000000u | d[pos] << 8 | d[pos+1]); pos += 2;
        }
        else if ((cmd & 0xF0) == 0x80) {
            uint8_t b = dac_ptr < dac_bank.size() ? dac_bank[dac_ptr] : 0;
            dac_ptr++;
            cmds.push_back(0xD0000000u | 0x2A00u | b);
            wait_ticks(cmd & 0xF);
        }
        else if (cmd == 0xE0) { dac_ptr = rd32(d, pos); pos += 4; }
        else if (cmd == 0xA0) { cmds.push_back(0x20000000u | (d[pos] & 15) << 8 | d[pos+1]); pos += 2; }
        else if (cmd == 0xB4) {
            if (d[pos] > 0x1F) stream_warn++;  // FDS и пр. не поддержаны
            else cmds.push_back(0x90000000u | d[pos] << 8 | d[pos+1]);
            pos += 2;
        }
        else if (cmd == 0xC0) {
            uint32_t off = d[pos] | d[pos+1] << 8;
            if (off > 0xFF) pcm_masked++;
            cmds.push_back(0x30000000u | (off & 0xFF) << 8 | d[pos+2]);
            pos += 3;
        }
        else if (cmd == 0x61) { wait_ticks(d[pos] | d[pos+1] << 8); pos += 2; }
        else if (cmd == 0x62) wait_ticks(735);
        else if (cmd == 0x63) wait_ticks(882);
        else if ((cmd & 0xF0) == 0x70) wait_ticks((cmd & 15) + 1);
        else if (cmd == 0x66) run = false;
        else if (cmd == 0x67) {
            uint8_t kind = d[pos + 1];
            uint32_t len = rd32(d, pos + 2) & 0x7FFFFFFF;
            size_t body = pos + 6;
            if (kind == 0x00) {
                dac_bank.insert(dac_bank.end(), d.begin() + body, d.begin() + body + len);
            } else if (kind == 0x80 && len >= 8) {
                RomBlock b;
                b.start = rd32(d, body + 4);
                b.bytes.assign(d.begin() + body + 8, d.begin() + body + len);
                rom_blocks.push_back(std::move(b));
            } else if (kind == 0x04) {
                adpcm_blocks.push_back({(uint32_t)adpcm_bank.size(), len});
                adpcm_bank.insert(adpcm_bank.end(), d.begin() + body, d.begin() + body + len);
            } else if ((kind == 0x8E || kind == 0x8B) && len >= 8) {
                // ROM сэмплов K053260 (0x8E) / OKIM6295 (0x8B):
                // [u32 полный размер][u32 смещение][данные]
                RomBlock b;
                b.start = (kind == 0x8E ? 0x200000u : 0x100000u) + rd32(d, body + 4);
                b.bytes.assign(d.begin() + body + 8, d.begin() + body + len);
                rom_blocks.push_back(std::move(b));
            } else if (kind == 0xC2 && len >= 2) {
                // DPCM-страница NES: [u16 адрес][данные] — синхронно с потоком
                uint32_t a = (d[body] | d[body+1] << 8) & 0x7FFF;
                cmds.push_back(0xA0000000u | a);
                for (uint32_t i = 2; i < len; i++) cmds.push_back(0xB0000000u | d[body + i]);
            }
            pos += 6 + len;
        }
        else if (cmd == 0xB7) { cmds.push_back(0x40000000u | (d[pos] & 3) << 8 | d[pos+1]); pos += 2; }
        else if (cmd == 0x93) {
            uint32_t start = rd32(d, pos + 1);
            uint8_t lm = d[pos + 5];
            uint32_t ll = rd32(d, pos + 6);
            uint32_t len = lm == 1 ? ll
                         : lm == 3 ? (uint32_t)adpcm_bank.size() - start
                         : (stream_warn++, 0);
            cmds.push_back(0x50000000u | (ADPCM_BASE + start));
            if (len) cmds.push_back(0x60000000u | (len & 0xFFFFFF));
            pos += 10;
        }
        else if (cmd == 0x94) { cmds.push_back(0x70000000u); pos += 1; }
        else if (cmd == 0x95) {
            uint16_t blk = d[pos + 1] | d[pos + 2] << 8;
            if (blk < adpcm_blocks.size()) {
                cmds.push_back(0x50000000u | (ADPCM_BASE + adpcm_blocks[blk].off));
                cmds.push_back(0x60000000u | (adpcm_blocks[blk].len & 0xFFFFFF));
            } else stream_warn++;
            pos += 4;
        }
        else if (cmd >= 0x51 && cmd <= 0x5F) pos += 2;
        else if (cmd == 0x68) pos += 11;
        else if (cmd >= 0x90 && cmd <= 0x95) { static const int L[] = {4,4,5,10,1,4}; pos += L[cmd-0x90]; }
        else if (cmd >= 0xA0 && cmd <= 0xBF) pos += 2;
        else if (cmd >= 0xC0 && cmd <= 0xDF) pos += 3;
        else if (cmd >= 0xE0) pos += 4;
        else { fprintf(stderr, "неизвестная команда 0x%02x\n", cmd); return 1; }
        if (max_seconds > 0 && total_ticks >= max_seconds * 44100) break;
    }
    size_t rom_bytes = 0;
    for (auto& b : rom_blocks) rom_bytes += b.bytes.size();
    fprintf(stderr, "VGM: YM2151 @ %u Гц, AY @ %u Гц, SegaPCM @ %u Гц (ROM %zu Б, блоков %zu), "
            "MSM6258 @ %u Гц (флаги 0x%02x, ADPCM-банк %zu Б, блоков %zu), NES APU @ %u Гц, %zu команд, %.1f c\n",
            ym_clk, ay_clk, pcm_clk, rom_bytes, rom_blocks.size(),
            adpcm_clk, adpcm_flags, adpcm_bank.size(), adpcm_blocks.size(),
            nes_clk, cmds.size(), total_ticks / 44100.0);
    if (pcm_masked) fprintf(stderr, "ВНИМАНИЕ: %zu записей SegaPCM с offset > 0xFF (замаскированы)\n", pcm_masked);
    if (stream_warn) fprintf(stderr, "ВНИМАНИЕ: %zu необработанных DAC-стрим команд\n", stream_warn);

    Tb tb;
    // фазовые инкременты cen: Fchip / CLK_HZ * 2^32
    if (ym_clk) tb.wb(3, true, (uint32_t)((double)ym_clk / CLK_HZ * 4294967296.0 + 0.5));
    if (ay_clk) tb.wb(4, true, (uint32_t)((double)ay_clk / CLK_HZ * 4294967296.0 + 0.5));
    if (pcm_clk) {
        double inc = (double)pcm_clk * 2.0 / CLK_HZ * 4294967296.0;
        tb.wb(5, true, inc >= 4294967295.0 ? 0xFFFFFFFFu : (uint32_t)(inc + 0.5));
    }
    // Гейны: неиспользуемые чипы глушим (idle-DC/шум не попадает в микс);
    // SegaPCM 34/64 — баланс Out Run по MAME (0.30 FM / 0.70 PCM)
    tb.wb(6, true, (adpcm_clk ? 64u : 0u) << 24 | (pcm_clk ? 34u : 0u) << 16
                 | (ay_clk ? 64u : 0u) << 8 | (ym_clk ? 64u : 0u));
    tb.wb(0xC, true, (opl_clk ? 16u : 0u) << 24 | (nes_clk ? 64u : 0u));
    if (opl_clk) tb.wb(0x14, true, (uint32_t)((double)opl_clk / CLK_HZ * 4294967296.0 + 0.5));
    if (scc_clk) {
        tb.wb(0x21, true, (uint32_t)((double)scc_clk / CLK_HZ * 4294967296.0 + 0.5));
        tb.wb(0x22, true, 64);
    }
    if (k060_clk) {
        tb.wb(0x25, true, (uint32_t)((double)k060_clk / CLK_HZ * 4294967296.0 + 0.5));
        tb.wb(0x26, true, 64);
    }
    if (adpcm_clk) {
        double inc = (double)adpcm_clk / CLK_HZ * 4294967296.0;
        tb.wb(7, true, inc >= 4294967295.0 ? 0xFFFFFFFFu : (uint32_t)(inc + 0.5));
        tb.wb(0xA, true, adpcm_flags & 3);
    }
    if (nes_clk) tb.wb(0xB, true, (uint32_t)((double)nes_clk / CLK_HZ * 4294967296.0 + 0.5));
    if (fm_clk) {
        double inc = (double)fm_clk / CLK_HZ * 4294967296.0;
        tb.wb(0x16, true, inc >= 4294967295.0 ? 0xFFFFFFFFu : (uint32_t)(inc + 0.5));
    }
    if (sn_clk) tb.wb(0x17, true, (uint32_t)((double)sn_clk / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0x15, true, (sn_clk ? 32u : 0u) << 8 | (fm_clk ? 64u : 0u));
    tb.wb(2, true, 1);                       // сброс чипа (чистит FIFO!)
    // разблокировка регистров звука SCC (BR2=0x3F) — только после сброса
    if (scc_clk) tb.wb(0, true, 0xF0000000u | (7u << 16));
    for (int i = 0; i < 2048; i++) tb.step(); // дать сбросу пройти

    // Загрузка сэмпл-ROM и ADPCM-банка через WB (как это будет делать фирмварь)
    for (auto& b : rom_blocks) {
        tb.wb(8, true, b.start);
        for (size_t i = 0; i < b.bytes.size(); i += 2) {
            uint32_t w = b.bytes[i] | (i + 1 < b.bytes.size() ? b.bytes[i+1] << 8 : 0);
            tb.wb(9, true, w);
        }
    }
    if (!adpcm_bank.empty()) {
        tb.wb(8, true, ADPCM_BASE);
        for (size_t i = 0; i < adpcm_bank.size(); i += 2) {
            uint32_t w = adpcm_bank[i] | (i + 1 < adpcm_bank.size() ? adpcm_bank[i+1] << 8 : 0);
            tb.wb(9, true, w);
        }
    }

    // Стриминг с контролем заполнения FIFO — как фирмварь
    size_t fed = 0;
    while (fed < cmds.size()) {
        uint32_t used = tb.fifo_used();
        if (used < 1536) {
            size_t batch = std::min(cmds.size() - fed, (size_t)(2000 - used));
            for (size_t i = 0; i < batch; i++) tb.wb(0, true, cmds[fed++]);
        } else {
            for (int i = 0; i < 20000; i++) tb.step();
        }
    }
    while (tb.seq_busy()) for (int i = 0; i < 20000; i++) tb.step();

    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    fprintf(stderr, "готово: %zu сэмплов @ %u Гц → %s\n", tb.pcm.size() / 2, rate, out);
    return 0;
}

static void write_wav_file(const char* out, const std::vector<int16_t>& pcm, uint32_t rate) {
    FILE* f = fopen(out, "wb");
    if (!f) { fprintf(stderr, "не открыть %s\n", out); exit(1); }
    uint32_t dlen = pcm.size() * 2, riff = 36 + dlen, byterate = rate * 4, fmtlen = 16;
    uint16_t fmt16[] = {1, 2}, block[] = {4, 16};
    fwrite("RIFF", 4, 1, f); fwrite(&riff, 4, 1, f); fwrite("WAVEfmt ", 8, 1, f);
    fwrite(&fmtlen, 4, 1, f); fwrite(fmt16, 4, 1, f); fwrite(&rate, 4, 1, f);
    fwrite(&byterate, 4, 1, f); fwrite(block, 4, 1, f);
    fwrite("data", 4, 1, f); fwrite(&dlen, 4, 1, f);
    fwrite(pcm.data(), 2, pcm.size(), f);
    fclose(f);
}
