// chipbox_tb: интеграционный тест секвенсора chipbox.
// Играет .vgm/.vgz через Wishbone-интерфейс (как это будет делать фирмварь)
// и пишет WAV с аудио-выхода. Проверяет: FIFO, тайминг тиков, busy-протокол.
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cmath>
#include <vector>
#include <map>
#include <algorithm>
#include <zlib.h>
#include "Vchipbox.h"
#include "verilated.h"

// Должно совпадать с параметром CLK_HZ при верилировании (-GCLK_HZ)
#ifndef M4_CLK_HZ
#define M4_CLK_HZ 8000000.0
#endif
static const double CLK_HZ = M4_CLK_HZ;

// Инкремент фазы для регистров частоты чипов. Аккумулятор 32-битный, и
// если частота чипа выше тактовой ядра, значение переполняется и молча
// становится дробной частью — домен уезжает в разы. Так и вышло с Game
// Boy: 8.388608 МГц при тактовой стенда 8 МГц давали долю 0.049 вместо
// 1.049, то есть домен работал в 21 раз медленнее, и все замеры GBS в
// симуляции были недействительны. Лучше упасть, чем измерять не то.
static uint32_t phase_inc(double hz, const char* what) {
    double v = hz / CLK_HZ * 4294967296.0;
    if (v >= 4294967296.0) {
        fprintf(stderr, "ОШИБКА: %s требует %.0f Гц при тактовой стенда %.0f Гц — "
                "инкремент фазы переполнится. Соберите с большей -GCLK_HZ.\n",
                what, hz, CLK_HZ);
        exit(2);
    }
    return (uint32_t)(v + 0.5);
}


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
    // Чтение с УДЕРЖАНИЕМ stb на несколько тактов.
    //
    // Обычный wb() снимает stb сразу после ack и потому не воспроизводит
    // поведение настоящего мастера: ack у нас — импульс на каждый такт,
    // пока подняты stb и cyc, значит блок чтения исполняется столько раз,
    // сколько мастер держит шину. Для регистров с побочным действием (VU
    // очищается по чтению) это меняет результат, и симуляция расходится с
    // железом. Нужно, чтобы проверить именно это.
    uint32_t wb_hold(uint32_t word_addr, int hold) {
        top.addr = word_addr; top.we = 0; top.data_write = 0;
        top.stb = 1; top.cyc = 1;
        int guard = 100;
        do { step(); } while (!top.ack && --guard);
        uint32_t first = top.data_read;
        for (int i = 1; i < hold; i++) step();
        uint32_t last = top.data_read;
        top.stb = 0; top.cyc = 0;
        step();
        return hold > 1 ? last : first;
    }
    uint32_t fifo_used() { return wb(1, false) & 0x1FFF; }
    bool seq_busy() { return (wb(1, false) >> 29) & 1; }
};

// Выходной каскад железа.
//
// chipbox отдаёт сэмплы своим стробом, а i2s в audio.sv читает последний
// защёлкнутый на 48 кГц — то есть выборка с удержанием, без интерполяции
// и без фильтра. Стенд писал WAV ДО этого каскада, и все спектральные
// сравнения с эталоном за всю историю проекта делались по сигналу,
// которого на выходе не бывает. Пока строб был 55029 Гц, каскад
// выбрасывал 7029 сэмплов в секунду и рождал неармонические призраки на
// |55029-48000| +- f: тон 5 кГц давал 2029 Гц на -20 дБ, тон 8 кГц — 971
// Гц на -15 дБ. Уровень при этом падал на 0.1-0.6 дБ, поэтому ни один из
// наших признаков (уровень, полосы, огибающая) этого не видел.
//
// Сейчас строб равен 48 кГц (OUT_DIV в chipbox.sv), и на тактовой железа
// каскад — тождество. Но быстрый стенд на 8 МГц делит свою тактовую
// иначе, поэтому приводим здесь: WAV всегда равен тому, что выходит из
// наушников, на любой тактовой стенда. Отключается --no-out-stage, если
// нужно посмотреть сигнал до каскада.
static const uint32_t OUT_RATE = 48000;
static bool out_stage = true;

// Режим выходного тракта NES (регистр 0x2D): 0 NES, 1 Famicom, 3 выключен.
// По умолчанию выключен — ровно как в меню ядра, чтобы стенд играл то же,
// что железо. Для замеров включается ключом --nes-filter.
static uint32_t nes_flt_opt = 3;
// Режим вывода, как пункт «Output» в меню ядра (регистр 0x30):
// 0 стерео, 1 моно (--mono), 2 суженная сцена (--narrow).
static uint32_t mono_opt = 0;
// Гейн APU для замеров запаса по уровню. По умолчанию 0 — берётся значение
// фирмвари (80). Ключ --apu-gain N позволяет снять пик БЕЗ ограничения:
// на 120 громкие рипы NES упираются в шкалу, и настоящий пик не виден.
static uint32_t apu_gain_opt = 0;
// То же для HuC6280 (регистр 0x28), ключ --huc-gain N. Нужен по той же
// причине: на рабочем множителе громкие рипы с CD упираются в шкалу, и
// настоящий пик из рендера не виден.
static uint32_t huc_gain_opt = 0;
static uint32_t opll_gain_opt = 0;   // --opll-gain: гейн OPLL (YM2413/VRC7) на OPL3
static uint32_t pwm_gain_opt = 64;   // --pwm-gain: гейн PWM 32X

static std::vector<int16_t> to_out_rate(const std::vector<int16_t>& pcm, uint32_t rate) {
    size_t n_in = pcm.size() / 2;
    size_t n_out = (size_t)((double)n_in * OUT_RATE / rate);
    std::vector<int16_t> o;
    o.reserve(n_out * 2);
    for (size_t k = 0; k < n_out; k++) {
        size_t i = (size_t)((double)k * rate / OUT_RATE);
        if (i >= n_in) i = n_in - 1;
        o.push_back(pcm[2 * i]);
        o.push_back(pcm[2 * i + 1]);
    }
    return o;
}

static void write_wav_file(const char* out, const std::vector<int16_t>& pcm, uint32_t rate);

// Глушит в микшере все чипы разом.
//
// Каждый путь обнулял только те гейны, о которых знал, а у остальных
// оставался сброс 64 — и незанятый чип подмешивал в сумму свой холостой
// уровень. На NSF это давало постоянную -51 там, где эталон выдаёт
// ровный ноль: на тихих местах такая добавка перевешивала музыку.
static void mute_all(Tb& tb) {
    tb.wb(6, true, 0);      // ADPCM, SegaPCM, AY, YM2151
    tb.wb(0xC, true, 0);    // OPL, SID, Game Boy, NES APU
    tb.wb(0x15, true, 0);   // SN76489, YM2612
    tb.wb(0x22, true, 0);   // SCC
    tb.wb(0x24, true, 0);   // OKIM6295 (вместе с признаком ss)
    tb.wb(0x26, true, 0);   // K053260
    tb.wb(0x28, true, 0);   // HuC6280
}

// Сторож выходного каскада: строб микса ОБЯЗАН совпадать с частотой i2s.
//
// Пока они расходились (55029 против 48000), между ними стояла выборка с
// удержанием, засевавшая всю полосу неармоническими призраками до -15 дБ,
// и ни один наш признак этого не показывал. Проверка дешёвая — 0.2 с
// модельного времени с заглушенными чипами: считаем сами стробы.
//
// Строгая проверка возможна только на тактовой железа: 57120000/48000
// делится ровно, а 8 МГц быстрого стенда — нет.
static int outrate_selftest() {
    Tb tb;
    mute_all(tb);
    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    uint64_t cycles = (uint64_t)(0.2 * CLK_HZ);
    while (tb.cycle < cycles) tb.step();

    double got = (double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ);
    uint32_t div = (uint32_t)(CLK_HZ / 48000.0);   // OUT_DIV в chipbox.sv
    double want = CLK_HZ / (double)div;
    fprintf(stderr, "строб микса: %.1f Гц (делитель %u даёт %.1f), i2s: %u Гц\n",
            got, div, want, OUT_RATE);

    if (fabs(got - want) > 5.0) {
        fprintf(stderr, "строб не равен CLK/(CLK/48000): значит OUT_DIV в "
                "chipbox.sv считается уже не от частоты i2s -> FAIL\n");
        return 1;
    }
    if ((uint64_t)CLK_HZ == 57120000ull) {
        if (fabs(got - (double)OUT_RATE) > 1.0) {
            fprintf(stderr, "на тактовой железа строб ОБЯЗАН быть %u Гц: "
                    "иначе между chipbox и i2s снова встанет пересчёт -> FAIL\n", OUT_RATE);
            return 1;
        }
        fprintf(stderr, "сторож выходного каскада: OK\n");
    } else {
        fprintf(stderr, "тактовая стенда не железная — строгую проверку пропускаю, "
                "перепроверьте с CLK=57120000\n");
    }
    return 0;
}

// Чтение VU не должно зависеть от того, сколько мастер держит шину.
//
// С устройства пришло «полоска не шевелится», притом что таймер идёт и
// музыка играет — то есть чтение регистра 0x18 работает, а 0x1A отдаёт
// ноль. Подозрение: ack у нас импульс на КАЖДЫЙ такт с поднятым stb, и
// блок чтения исполняется столько раз, сколько мастер держит шину. У
// 0x1A есть побочное действие (очистка пиков), поэтому при удержании
// шины мастер получает уже очищенное значение. Наш wb() снимает stb
// сразу и потому проблемы не видел.
static int vu_selftest() {
    Tb tb;
    mute_all(tb);
    tb.wb(0xC, true, 64);   // гейн APU
    tb.wb(0xB, true, phase_inc(1789773.0, "NES APU"));
    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    const uint32_t regs[][2] = {{0x15,0x0F},{0x00,0xBF},{0x01,0x00},{0x02,0xFD},{0x03,0x00}};
    for (auto& r : regs) tb.wb(0, true, 0x90000000u | r[0] << 8 | r[1]);
    while (tb.cycle < (uint64_t)(0.2 * CLK_HZ)) tb.step();

    // Окно накопления перед каждым замером одинаковое: чтение обнуляет
    // пики, и без выравнивания значения нельзя сравнивать между собой —
    // а сравнивать надо, иначе тест ловит только полный ноль.
    int bad = 0;
    uint32_t ref = 0;
    for (int hold : {1, 2, 3, 4, 8}) {
        tb.wb(0x1A, false);                          // сбросить пики
        // Окно задаётся ВРЕМЕНЕМ, а не тактами. В тактах оно на разных
        // тактовых стенда получается разной длины: 20000 тактов — это
        // 120 выходных отсчётов на 8 МГц и всего 17 на 57.12, а период
        // тона около 109 отсчётов. Пик ловился в случайной фазе, и тест
        // падал на ровном месте, показывая то 4094, то 1243.
        uint64_t until = tb.cycle + (uint64_t)(0.02 * CLK_HZ);
        while (tb.cycle < until) tb.step();
        uint32_t v = tb.wb_hold(0x1A, hold) & 0xFFFF;
        if (!ref) ref = v;
        bool ok = v && ref && v * 10 >= ref * 9 && v * 9 <= ref * 10;
        fprintf(stderr, "удержание stb %d такт(ов): VU=%u%s\n", hold, v,
                ok ? "" : (v ? "  <-- РАСХОДИТСЯ" : "  <-- НОЛЬ"));
        if (!ok) bad++;
    }
    fprintf(stderr, "селфтест VU: %s\n",
            bad ? "FAIL — чтение зависит от длины цикла шины" : "OK");
    return bad ? 1 : 0;
}

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
// Стаб NSF: тот же, что строит фирмварь (main.rs, nsf-ветка).
// clear_ram — обнуление $0000-$07FF и $6000-$7FFF перед INIT, как того
// требует спека NSF. Без него вторая песня подряд вешает INIT.
static std::vector<uint8_t> nsf_stub(const uint8_t* banks, bool banked,
                                     uint16_t init, uint16_t play,
                                     uint8_t song, bool clear_ram) {
    std::vector<uint8_t> b;
    auto put = [&](std::initializer_list<uint8_t> x) { b.insert(b.end(), x); };
    put({0x78, 0xD8});                         // SEI, CLD
    put({0xA2, 0xFF, 0x9A});                   // LDX #$FF, TXS
    if (clear_ram) {
        put({0xA9, 0x00, 0xAA});               // LDA #0, TAX
        uint8_t zp = (uint8_t)b.size();
        for (uint8_t p = 0; p < 8; p++) put({0x9D, 0x00, (uint8_t)(p)});  // STA $pp00,X
        put({0xE8});                                                     // INX
        put({0xD0, (uint8_t)((zp - (b.size() + 2)) & 0xFF)});            // BNE zp
        // WRAM $6000-$7FFF через указатель в zero page
        put({0xA9, 0x00, 0x85, 0x00, 0xA9, 0x60, 0x85, 0x01});           // ptr=$6000
        put({0xA2, 0x20, 0xA0, 0x00, 0xA9, 0x00});                       // X=32 стр, Y=0
        uint8_t wl = (uint8_t)b.size();
        put({0x91, 0x00, 0xC8});                                         // STA ($00),Y : INY
        put({0xD0, (uint8_t)((wl - (b.size() + 2)) & 0xFF)});            // BNE wl
        put({0xE6, 0x01, 0xCA});                                         // INC $01 : DEX
        put({0xD0, (uint8_t)((wl - (b.size() + 2)) & 0xFF)});            // BNE wl
        // APU в известное состояние: $4000-$4013 = 0, $4015=$0F, $4017=$40
        put({0xA2, 0x13, 0xA9, 0x00});
        uint8_t ap = (uint8_t)b.size();
        put({0x9D, 0x00, 0x40, 0xCA});                                   // STA $4000,X : DEX
        put({0x10, (uint8_t)((ap - (b.size() + 2)) & 0xFF)});            // BPL ap
        put({0xA9, 0x0F, 0x8D, 0x15, 0x40});
        put({0xA9, 0x40, 0x8D, 0x17, 0x40});
    }
    if (banked)
        for (int i = 0; i < 8; i++)
            put({0xA9, banks[i], 0x8D, (uint8_t)(0xF8 + i), 0x5F});
    put({0xA2, 0x00, 0xA0, 0x00});                       // LDX #0, LDY #0
    put({0xA9, song});                                   // LDA #песня
    put({0x20, (uint8_t)init, (uint8_t)(init >> 8)});    // JSR INIT
    uint8_t loop_at = (uint8_t)b.size();
    put({0xAD, 0xF0, 0x5F, 0xF0, 0xFB});                 // LDA $5FF0 : BEQ
    put({0x8D, 0xF0, 0x5F});                             // STA $5FF0
    put({0x20, (uint8_t)play, (uint8_t)(play >> 8)});    // JSR PLAY
    put({0x4C, loop_at, 0x50});                          // JMP loop
    b.push_back(0x40);                                   // RTI
    return b;
}

// Проигрывание одной песни: последовательность ровно как в фирмвари
static void nsf_start(Tb& tb, const std::vector<uint8_t>& stub, bool has5b) {
    uint8_t rti = (uint8_t)(stub.size() - 1);
    const uint8_t vecs[6] = {rti, 0x50, 0x00, 0x50, rti, 0x50};
    tb.wb(2, true, 0);
    for (size_t i = 0; i < stub.size(); i++) tb.wb(0xD, true, i << 8 | stub[i]);
    for (size_t i = 0; i < 6; i++) tb.wb(0xE, true, i << 8 | vecs[i]);
    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(6, true, has5b ? 64 << 8 : 0);
    tb.wb(0xC, true, 64);
    tb.wb(0x15, true, 0);
    tb.wb(2, true, 6);
}

// Проверка переключения песен: играем песню A, затем песню B тем же
// путём, что и фирмварь. Печатаем пик и счётчики p_acks/snd_wr (WB 0x1B).
static int nsf_songs(const char* path, const char* out, double seconds, bool clear_ram) {
    std::vector<uint8_t> d = read_maybe_gz(path);
    if (d.size() < 0x81 || memcmp(d.data(), "NESM\x1a", 5)) {
        fprintf(stderr, "не NSF: %s\n", path);
        return 1;
    }
    uint16_t load = d[0x08] | d[0x09] << 8;
    uint16_t init = d[0x0A] | d[0x0B] << 8;
    uint16_t play = d[0x0C] | d[0x0D] << 8;
    const uint8_t* banks = &d[0x70];
    bool banked = false;
    for (int i = 0; i < 8; i++) banked |= banks[i] != 0;
    uint8_t songs = d[6] ? d[6] : 1;
    fprintf(stderr, "NSF: %u песен, load=%04x init=%04x play=%04x, банки=%d, clear_ram=%d\n",
            songs, load, init, play, banked, clear_ram);

    Tb tb;
    tb.wb(0xB, true, (uint32_t)(1789773.0 / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0xF, true, (uint32_t)(60.0 / CLK_HZ * 4294967296.0 + 0.5));
    uint32_t base = 0x700000 + (banked ? (load & 0x0FFF) : (uint32_t)(load - 0x8000));
    tb.wb(8, true, base);
    for (size_t i = 0x80; i < d.size(); i += 2)
        tb.wb(9, true, d[i] | (i + 1 < d.size() ? d[i + 1] << 8 : 0));

    int bad = 0;
    for (uint8_t song = 0; song < (songs < 8 ? songs : 8); song++) {
        std::vector<uint8_t> stub = nsf_stub(banks, banked, init, play, song, clear_ram);
        if (stub.size() > 0x100) { fprintf(stderr, "стаб не влез: %zu\n", stub.size()); return 1; }
        size_t pcm0 = tb.pcm.size();
        nsf_start(tb, stub, false);
        uint64_t until = tb.cycle + (uint64_t)(seconds * CLK_HZ);
        while (tb.cycle < until) tb.step();
        uint32_t diag = tb.wb(0x1B, false);
        int16_t peak = 0;
        for (size_t i = pcm0 + (tb.pcm.size() - pcm0) / 2; i < tb.pcm.size(); i += 2)
            peak = std::max(peak, (int16_t)abs(tb.pcm[i]));
        fprintf(stderr, "  песня %u: пик=%5d  p_acks=%u snd_wr=%u  (стаб %zu байт)\n",
                song + 1, peak, diag & 0xFFFF, diag >> 16, stub.size());
        if (peak <= 500) bad++;
    }
    uint32_t rate = (uint32_t)((double)tb.pcm.size() / 2 / (tb.cycle / CLK_HZ) + 0.5);
    write_wav_file(out, tb.pcm, rate);
    fprintf(stderr, "nsf-songs: молчащих песен %d -> %s\n", bad, out);
    return bad ? 1 : 0;
}

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

    // Байт расширений $7B: бит 1 — VRC7. Транслятор OPLL получает записи
    // с шины 6502 ($9010/$9030), рег 0x34 = 3; OPL3 тактуется как OPL2;
    // гейн OPLL тот же, что в VGM-пути (см. фирмварь, OPLL_GAIN)
    bool has_vrc7 = (d[0x7B] & 0x02) != 0;
    Tb tb;
    mute_all(tb);
    tb.wb(0xC, true, 80 | (has_vrc7 ? (opll_gain_opt ? opll_gain_opt : 11u) << 24 : 0));
    tb.wb(0x34, true, has_vrc7 ? 3u : 0u);
    if (has_vrc7) tb.wb(0x14, true, (uint32_t)((double)(3579545ull * 64 / 9) / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0x2D, true, nes_flt_opt);   // выходной тракт NES, см. --nes-filter
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
    mute_all(tb);
    tb.wb(0xC, true, 80);    // канал APU (VRC6 подмешан в него), уровень см. в фирмвари
    tb.wb(0x2D, true, nes_flt_opt);
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
    for (size_t i = 0; i < sizeof prog; i++) tb.wb(0x11, true, (uint32_t)(0x400 + i) << 8 | prog[i]);
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
    for (size_t i = 0; i < sizeof prog_full; i++) tb.wb(0x11, true, (uint32_t)(0x400 + i) << 8 | prog_full[i]);
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
// Селфтест HuC6280: синус в волновую таблицу канала 0, проверяем тон
// на выходе микшера. Период P даёт 3579545/(32*(P+1)) Гц.
static int huc_selftest(const char* out, double seconds) {
    Tb tb;
    tb.wb(6, true, 0);            // прочие микс-каналы в ноль
    tb.wb(0xC, true, 0);
    tb.wb(0x15, true, 0);
    tb.wb(0x28, true, 64);        // huc_gain
    tb.wb(0x27, true, (uint32_t)(3579545.0 / CLK_HZ * 4294967296.0 + 0.5));

    tb.wb(2, true, 1);
    for (int i = 0; i < 2048; i++) tb.step();
    tb.wb(2, true, 0);
    for (int i = 0; i < 1200; i++) tb.step();

    auto huc = [&](int reg, int data) {
        tb.wb(0, true, 0xF3000000u | ((reg & 0xF) << 8) | (data & 0xFF));
    };
    huc(0, 0);                    // канал 0
    huc(1, 0xFF);                 // общая громкость
    huc(4, 0x00);                 // канал выкл -> индекс волны с нуля
    for (int i = 0; i < 32; i++)  // синус, 5 бит без знака
        huc(6, (int)lround(15.5 + 15.0 * sin(2.0 * M_PI * i / 32.0)) & 0x1F);
    huc(2, 0xFC);                 // период lo
    huc(3, 0x01);                 // период hi -> 0x1FC = 508
    huc(5, 0xFF);                 // баланс
    huc(4, 0x9F);                 // канал вкл, громкость 31

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
    bool ok = peak > 1000 && hz > 185 && hz < 260;
    fprintf(stderr, "селфтест HuC6280: peak=%d, тон ~%.0f Гц (ждём ~220) → %s\n",
            peak, hz, ok ? "OK" : "FAIL");
    return ok ? 0 : 1;
}

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
// Отпускание перемотки: держим FF при НЕПУСТОМ FIFO (только так копится
// отставание цели), отпускаем и проверяем, что следующая пауза длится
// столько, сколько записано. Старый тест ff проверял лишь ускорение и
// эту ошибку не видел: музыка неслась ещё секунду-две после снятия бита.
static int ff_release_selftest() {
    Tb tb;
    tb.wb(0x17, true, (uint32_t)(3579545.0 / CLK_HZ * 4294967296.0));
    tb.wb(6, true, 0); tb.wb(0xC, true, 0); tb.wb(0x15, true, 32u << 8);
    tb.wb(2, true, 1);
    for (int i = 0; i < 4096; i++) tb.step();

    const uint32_t W = 147;              // 1/300 c на паузу
    // Перемотка при НЕПУСТОМ FIFO: только так копится отставание цели.
    // Стоит FIFO опустеть — срабатывает штатный ресинк и стирает его.
    tb.wb(2, true, 0x40);
    uint64_t until = tb.cycle + (uint64_t)(1.0 * CLK_HZ);
    while (tb.cycle < until) {
        if ((tb.wb(1, false) & 0x1FFF) < 400)
            for (int i = 0; i < 200; i++) tb.wb(0, true, 0x80000000u | W);
        tb.step();
    }
    uint32_t left = tb.wb(1, false) & 0x1FFF;   // очередь на момент отпускания
    tb.wb(2, true, 0);

    double nominal = (double)left * W / 44100.0;
    uint64_t t0 = tb.cycle, cap = t0 + (uint64_t)((nominal * 3 + 1.0) * CLK_HZ);
    while (tb.seq_busy() && tb.cycle < cap) tb.step();
    double took = (double)(tb.cycle - t0) / CLK_HZ;

    // без ограничения отставания очередь слетает почти мгновенно
    bool ok = took > nominal * 0.7;
    fprintf(stderr, "селфтест ff-release: в очереди %u пауз (%.2f c), слились за %.2f c → %s\n",
            left, nominal, took, ok ? "OK" : "FAIL");
    return ok ? 0 : 1;
}

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
static int gbs_file(const char* path, const char* out, double seconds, double gb_hz = 8388608.0, int song_opt = -1) {
    FILE* f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "не открыть %s\n", path); return 1; }
    std::vector<uint8_t> d;
    uint8_t buf[65536];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0) d.insert(d.end(), buf, buf + n);
    fclose(f);
    if (d.size() < 0x70 || memcmp(d.data(), "GBS", 3)) { fprintf(stderr, "не GBS\n"); return 1; }

    // --gbsong считается с единицы, как в заголовке GBS и в gbsplay.
    // Раньше ключ был с нуля, и сравнение с эталоном шло по разным
    // мелодиям: уровни похожи, огибающая не совпадает ни при какой
    // задержке, и это легко принять за дефект звука.
    uint8_t song = song_opt > 0 ? (uint8_t)(song_opt - 1) : (d[0x05] ? d[0x05] - 1 : 0);
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
    // тело так же в BRAM ядра: ROM теперь читается оттуда
    for (size_t i = 0x70; i < d.size() && load + (i - 0x70) < 0x8000; i++)
        tb.wb(0x11, true, (uint32_t)(load + (i - 0x70)) << 8 | d[i]);
    // и сверка обратным чтением — тем же путём, каким это делает фирмварь
    {
        int bad = 0, checked = 0;
        for (size_t i = 0x70; i < d.size() && load + (i - 0x70) < 0x8000; i += 61) {
            tb.wb(0x29, true, (uint32_t)(load + (i - 0x70)) & 0x7FFF);
            if ((tb.wb(0x29, false) & 0xFF) != d[i]) bad++;
            checked++;
        }
        fprintf(stderr, "сверка BRAM: расхождений %d из %d\n", bad, checked);
    }

    double play_hz = 59.73;
    if (tac & 4) {
        double base = (tac & 3) == 0 ? 4096.0 : (tac & 3) == 1 ? 262144.0
                     : (tac & 3) == 2 ? 65536.0 : 16384.0;
        play_hz = base / (256 - tma);
    }
    tb.wb(0xF, true, (uint32_t)(play_hz / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0x10, true, phase_inc(gb_hz, "клок Game Boy")); // gb_clk: по умолчанию как на железе (8.388608 МГц -> ÷2 = 4.194 МГц)
    mute_all(tb);
    tb.wb(0xC, true, 46u << 8);   // как в фирмвари, см. GB_GAIN

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
    // APU до INIT: NR52 вкл, NR51 панорама, NR50 громкость — как в фирмвари
    stub[o++] = 0x3E; stub[o++] = 0x80; stub[o++] = 0xE0; stub[o++] = 0x26;
    stub[o++] = 0x3E; stub[o++] = 0xFF; stub[o++] = 0xE0; stub[o++] = 0x25;
    stub[o++] = 0x3E; stub[o++] = 0x77; stub[o++] = 0xE0; stub[o++] = 0x24;
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
    {
        // Карта затронутых регистров $FF10-$FF3F — по одной строке на
        // канал, чтобы видеть, чем два рипа отличаются
        uint64_t m = (uint64_t)tb.wb(0x2A, false) | ((uint64_t)tb.wb(0x2B, false) << 32);
        static const char* grp[5] = {"CH1 $FF10-14", "CH2 $FF15-19", "CH3 $FF1A-1E",
                                     "CH4 $FF1F-23", "упр $FF24-26"};
        static const int lo[5] = {0x10, 0x15, 0x1A, 0x1F, 0x24}, hi[5] = {0x14, 0x19, 0x1E, 0x23, 0x26};
        for (int g = 0; g < 5; g++) {
            fprintf(stderr, "  %-14s", grp[g]);
            for (int a = lo[g]; a <= hi[g]; a++)
                fprintf(stderr, " %02X:%c", a, (m >> (a - 0x10)) & 1 ? '+' : '.');
            fprintf(stderr, "\n");
        }
        int wave = 0;
        for (int a = 0x30; a <= 0x3F; a++) wave += (m >> (a - 0x10)) & 1;
        fprintf(stderr, "  волновая таблица $FF30-3F: %d из 16 записано\n", wave);
    }
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
    mute_all(tb);
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
    double gb_hz_opt = 8388608.0;
    int gbs_song_opt = -1;
    Verilated::commandArgs(argc, argv);
    const char* in = nullptr; const char* out = "out.wav";
    double max_seconds = 0;
    bool selftest = false;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-o") && i + 1 < argc) out = argv[++i];
        else if (!strcmp(argv[i], "-t") && i + 1 < argc) max_seconds = atof(argv[++i]);
        else if (!strcmp(argv[i], "--nsf-selftest")) selftest = true;
        else if (!strcmp(argv[i], "--no-out-stage")) { out_stage = false; }
        else if (!strcmp(argv[i], "--nes-filter") && i + 1 < argc) { nes_flt_opt = atoi(argv[++i]); }
        else if (!strcmp(argv[i], "--mono")) { mono_opt = 1; }
        else if (!strcmp(argv[i], "--narrow")) { mono_opt = 2; }
        else if (!strcmp(argv[i], "--apu-gain") && i + 1 < argc) { apu_gain_opt = atoi(argv[++i]); }
        else if (!strcmp(argv[i], "--huc-gain") && i + 1 < argc) { huc_gain_opt = atoi(argv[++i]); }
        else if (!strcmp(argv[i], "--opll-gain") && i + 1 < argc) { opll_gain_opt = atoi(argv[++i]); }
        else if (!strcmp(argv[i], "--pwm-gain") && i + 1 < argc) { pwm_gain_opt = atoi(argv[++i]); }
        else if (!strcmp(argv[i], "--out-rate-selftest")) { return outrate_selftest(); }
        else if (!strcmp(argv[i], "--vu-selftest")) { return vu_selftest(); }
        else if (!strcmp(argv[i], "--apu-selftest")) { return apu_selftest("apu_st.wav", 1.0); }
        else if (!strcmp(argv[i], "--gbs-selftest")) { return gbs_selftest(out, 2.0); }
        else if (!strcmp(argv[i], "--gbs-int-selftest")) { return gbs_int_selftest(out, 2.0); }
        else if (!strcmp(argv[i], "--sid-selftest")) { return sid_selftest(out, 2.0); }
        else if (!strcmp(argv[i], "--cmds") && i + 1 < argc) { return play_cmds(argv[i+1], out); }
        else if (!strcmp(argv[i], "--pause-selftest")) { return pause_selftest(out); }
        else if (!strcmp(argv[i], "--reset-selftest")) { return reset_selftest(out); }
        else if (!strcmp(argv[i], "--ff-release-selftest")) { return ff_release_selftest(); }
        else if (!strcmp(argv[i], "--ff-selftest")) { return ff_selftest(); }
        else if (!strcmp(argv[i], "--vrc6-selftest")) { return vrc6_selftest(out, 1.0); }
        else if (!strcmp(argv[i], "--huc-selftest")) { return huc_selftest(out, max_seconds > 0 ? max_seconds : 1.0); }
        else if (!strcmp(argv[i], "--scc-selftest")) { return scc_selftest(out, 1.0); }
        else if (!strcmp(argv[i], "--okim-selftest")) { return okim_selftest(out, 1.0); }
        else if (!strcmp(argv[i], "--k060-selftest")) { return k060_selftest(out, 1.0); }
        else if (!strcmp(argv[i], "--gbhz") && i + 1 < argc) { gb_hz_opt = atof(argv[++i]); }
        else if (!strcmp(argv[i], "--gbsong") && i + 1 < argc) { gbs_song_opt = atoi(argv[++i]); }
        else if (!strcmp(argv[i], "--gbsfile") && i + 1 < argc) { return gbs_file(argv[++i], out, max_seconds > 0 ? max_seconds : 4.0, gb_hz_opt, gbs_song_opt); }
        else if (!strcmp(argv[i], "--nsf-songs") && i + 1 < argc) { return nsf_songs(argv[++i], out, max_seconds > 0 ? max_seconds : 3.0, true); }
        else if (!strcmp(argv[i], "--nsf-songs-noclear") && i + 1 < argc) { return nsf_songs(argv[++i], out, max_seconds > 0 ? max_seconds : 3.0, false); }
        else if (!strcmp(argv[i], "--nsffile") && i + 1 < argc) { return nsf_file(argv[++i], out, max_seconds > 0 ? max_seconds : 4.0); }
        else if (!strcmp(argv[i], "--sidfile") && i + 1 < argc) { return sid_file(argv[++i], out, max_seconds > 0 ? max_seconds : 4.0); }
        else in = argv[i];
    }
    if (selftest) return nsf_selftest(out, max_seconds > 0 ? max_seconds : 2.0);
    if (!in) {
        fprintf(stderr, "usage: chipbox_tb <file.vgm|.vgz> [-o out.wav] [-t sec] "
                "[--no-out-stage] | --nsf-selftest | --out-rate-selftest\n"
                "  WAV пишется на частоте выхода i2s (48 кГц), как на железе;\n"
                "  --no-out-stage оставляет строб микса как есть. Ключ должен\n"
                "  идти ДО ключей, которые сразу запускают режим.\n");
        return 1;
    }

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
    uint32_t gb_clk_hdr = rd32(d, 0x80) & 0x3FFFFFFF;
    uint32_t fm_clk = rd32(d, 0x2C) & 0x3FFFFFFF;
    uint32_t sn_clk = rd32(d, 0x0C) & 0x3FFFFFFF;
    bool sn_dual = (rd32(d, 0x0C) & 0x40000000) != 0;
    uint8_t sn_att[2][4] = {{15,15,15,15},{15,15,15,15}};
    // Стерео SN76489: T6W28 объявлен в заголовке, Game Gear узнаётся по
    // первой маске в потоке. Модель та же, что в фирмвари.
    bool sn_t6w28 = (rd32(d, 0x0C) & 0x80000000) != 0;
    bool sn_stereo = sn_t6w28;
    uint8_t gg_mask = 0xFF;
    uint32_t rf5c_ptr = 0xFFFF;
    std::vector<uint8_t> rf5c_bank;   // блоки типа 0x01/0x02, источник для 0x68
    // Окно записи в ОЗУ — 4 КБ, страницу задаёт регистр 0x07 при сброшенном
    // бите 6. Оно накладывается на ВСЕ пути записи: и на команды 0xC1/0xC2,
    // и на блоки 0xC0/0xC1, и на заливку 0x68 (libvgm, DoRAMOfsPatches).
    // Без этого рип Sonic CD лил все сэмплы в первые 4 КБ поверх друг друга.
    uint32_t rf5c_wbank = 0;
    size_t rf5c_regs = 0, rf5c_ram = 0;
    size_t pos = data_off;
    // OPL-семейство: YM3812 (0x50), YM3526 (0x54), YMF262 (0x5C).
    // Полуклок ядра: 25.4545 МГц при номинале, масштаб от тактовой файла
    // (OPL2 x64/9, OPL3 x16/9) — см. разбор в фирмвари.
    uint32_t ym3812_clk = hdr_end >= 0x54 ? rd32(d, 0x50) & 0x3FFFFFFF : 0;
    uint32_t ym3526_clk = hdr_end >= 0x58 ? rd32(d, 0x54) & 0x3FFFFFFF : 0;
    uint32_t ymf262_clk = hdr_end >= 0x60 ? rd32(d, 0x5C) & 0x3FFFFFFF : 0;
    // OPLL (YM2413, бит 31 — VRC7): транслятор в chipbox переводит в OPL2,
    // тактовая как у OPL2; при наличии настоящего OPL в файле OPLL молчит
    uint32_t ym2413_clk = rd32(d, 0x10) & 0x3FFFFFFF;
    bool vrc7 = (rd32(d, 0x10) & 0x80000000u) != 0;
    bool opll = ym2413_clk && !ymf262_clk && !ym3812_clk && !ym3526_clk;
    uint32_t opl_clk = ymf262_clk ? (uint32_t)((uint64_t)ymf262_clk * 16 / 9)
                     : ym3812_clk ? (uint32_t)((uint64_t)ym3812_clk * 64 / 9)
                     : ym3526_clk ? (uint32_t)((uint64_t)ym3526_clk * 64 / 9)
                     : ym2413_clk ? (uint32_t)((uint64_t)ym2413_clk * 64 / 9) : 0;
    uint32_t scc_clk  = hdr_end >= 0xA0 ? rd32(d, 0x9C) & 0x3FFFFFFF : 0;
    uint32_t k060_clk = hdr_end >= 0xB0 ? rd32(d, 0xAC) & 0x3FFFFFFF : 0;
    uint32_t huc_clk  = hdr_end >= 0xA8 ? rd32(d, 0xA4) & 0x3FFFFFFF : 0;
    // OPN: FM уходит на YM2612, SSG на jt49; делители FM 1/6, SSG 1/4
    uint32_t ym2608_clk = hdr_end >= 0x4C ? rd32(d, 0x48) & 0x3FFFFFFF : 0;
    uint32_t ym2203_clk = hdr_end >= 0x48 ? rd32(d, 0x44) & 0x3FFFFFFF : 0;
    uint32_t opn_clk = ym2608_clk ? ym2608_clk : ym2203_clk;
    uint32_t pwm_clk = rd32(d, 0x70) & 0x3FFFFFFF;   // PWM 32X
    uint32_t rf5c_clk = rd32(d, 0x6C) & 0x3FFFFFFF;
    if (!rf5c_clk) rf5c_clk = rd32(d, 0x40) & 0x3FFFFFFF;   // родич RF5C68

    if (!ym_clk && !ay_clk && !pcm_clk && !adpcm_clk && !nes_clk && !fm_clk && !sn_clk && !opl_clk
        && !scc_clk && !k060_clk && !huc_clk && !opn_clk && !gb_clk_hdr
        && !rf5c_clk && !ym2413_clk && !pwm_clk) { fprintf(stderr, "в файле нет поддержанных чипов\n"); return 1; }

    // VGM → командные слова chipbox (это же будет делать фирмварь)
    // + отдельно собираем data-блоки SegaPCM ROM (тип 0x80)
    std::vector<uint32_t> cmds;
    auto push_sn_att = [&]() {
        uint32_t l = 0, r = 0;
        for (int ch = 0; ch < 4; ch++) {
            uint8_t al = (gg_mask >> (4 + ch)) & 1 ? sn_att[0][ch] : 15;
            uint8_t ar = (gg_mask >> ch) & 1 ? sn_att[1][ch] : 15;
            l |= (uint32_t)al << (4 * ch);
            r |= (uint32_t)ar << (4 * ch);
        }
        cmds.push_back(0xF5000000u | l);
        cmds.push_back(0xF5000000u | 1u << 16 | r);
    };
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
    // Громкости, которые просит сам файл. Разбор и модель — те же, что в
    // фирмвари (firmware/vgm-core/src/lib.rs), а туда сняты с libvgm.
    // Держать это только в фирмвари нельзя: тогда стенд играет другой
    // баланс, чем железо, и сравнение с эталоном относится не к тому.
    int vol_mod = 0;
    if (hdr_end > 0x7C) {
        uint8_t v = d[0x7C];
        vol_mod = v <= 0xC0 ? v : (v == 0xC1 ? -0x40 : (int)v - 0x100);
    }
    struct ChipVol { uint8_t chip, inst; uint16_t raw; };
    std::vector<ChipVol> chip_vols;
    if (hdr_end >= 0xC0) {
        uint32_t rel = rd32(d, 0xBC);
        if (rel) {
            size_t xo = 0xBC + rel;
            if (xo + 12 <= d.size() && rd32(d, xo) >= 12) {
                uint32_t vrel = rd32(d, xo + 8);
                if (vrel) {
                    size_t vb = xo + 8 + vrel;
                    if (vb < d.size()) {
                        size_t n = d[vb];
                        for (size_t i = 0; i < n && vb + 5 + 4 * i <= d.size(); i++)
                            chip_vols.push_back({d[vb+1+4*i], (uint8_t)(d[vb+2+4*i] & 1),
                                                 (uint16_t)(d[vb+3+4*i] | d[vb+4+4*i] << 8)});
                    }
                }
            }
        }
    }
    // Базовые громкости чипов из libvgm — только для абсолютных записей
    static const uint16_t CHIP_BASE_VOL[32] = {
        0x80, 0x200, 0x100, 0x100, 0x180, 0xB0, 0x100, 0x80,
        0x80, 0x100, 0x100, 0x100, 0x100, 0x100, 0x100, 0x98,
        0x80, 0xE0, 0x100, 0xC0, 0x100, 0x40, 0x11E, 0x1C0,
        0x100, 0xA0, 0x100, 0x100, 0x100, 0x100, 0x100, 0x100,
    };
    static const uint32_t POW2_32[32] = {
        256, 262, 267, 273, 279, 285, 292, 298, 304, 311, 318, 325, 332, 339, 347, 354,
        362, 370, 378, 386, 395, 403, 412, 421, 431, 440, 450, 459, 470, 480, 490, 501,
    };
    uint32_t vol_scale;
    {
        int i = vol_mod >> 5;              // целая часть степени, вниз
        uint32_t frac = POW2_32[vol_mod & 31];
        vol_scale = i >= 0 ? frac << std::min(i, 8) : frac >> std::min(-i, 8);
    }
    auto chip_scale = [&](uint8_t chip) -> uint32_t {
        for (auto& e : chip_vols) {
            if (e.chip != chip || e.inst != 0) continue;
            if (e.raw & 0x8000) return e.raw & 0x7FFF;
            uint32_t base = CHIP_BASE_VOL[chip & 0x1F];
            if ((chip & 0x80) && (chip & 0x7F) == 0x06) base /= 2;
            return base ? (uint32_t)e.raw * 256 / base : 256;
        }
        return 256;
    };
    // Почиповые громкости пока НЕ применяем — см. APPLY_CHIP_VOLUMES в
    // фирмвари: наши базовые гейны подбирались по файлам, которые эти
    // поправки уже несут, и применить их сверху значит посчитать дважды.
    // Замер это подтвердил. Общий модификатор 0x7C применяется всегда.
    const bool apply_chip_volumes = true;
    auto gain_of = [&](uint32_t base, uint8_t chip) -> uint32_t {
        if (!base) return 0;
        uint32_t v = apply_chip_volumes ? base * chip_scale(chip) / 256 : base;
        v = v * vol_scale / 256;
        return v > 255 ? 255 : v;
    };

    size_t pcm_masked = 0, stream_warn = 0;
    // Отброшенное осознанно: записи второго экземпляра чипа (второго
    // экземпляра в железе нет) и маски стерео Game Gear. И то, и другое
    // раньше уходило в тишину без сообщения, а маска вдобавок глушила
    // канал шума, потому что попадала в PSG как байт данных.
    size_t drop2 = 0, drop_gg = 0;
    auto wait_ticks = [&](uint32_t n) { if (n) { cmds.push_back(0x80000000u | n); total_ticks += n; } };
    bool run = true;
    while (run && pos < d.size()) {
        uint8_t cmd = d[pos++];
        if (cmd == 0x54) { cmds.push_back(0x10000000u | d[pos] << 8 | d[pos+1]); pos += 2; }
        // Game Boy: регистры APU $FF10-$FF3F, в VGM адрес отсчитан от $FF10
        else if (cmd == 0xB3) {
            if (d[pos] & 0x80) drop2++;  // бит 7 — второй экземпляр, не адрес
            else cmds.push_back(0xF4000000u | (uint32_t)(d[pos] + 0x10) << 8 | d[pos+1]);
            pos += 2;
        }
        else if (cmd == 0x52) { cmds.push_back(0xD0000000u | d[pos] << 8 | d[pos+1]); pos += 2; }
        else if (cmd == 0x53) { cmds.push_back(0xD0000000u | 0x10000u | d[pos] << 8 | d[pos+1]); pos += 2; }
        // Маска стерео Game Gear (0x4F, 0x3F — для второго чипа): порт
        // 0x06, а НЕ регистр PSG. Раньше была склеена с 0x50/0x30 и уходила
        // в чип как байт данных: обычное значение 0xFF для SN76489 — это
        // latch «канал 3, громкость, аттенюация 15», то есть глушило шум.
        // Стерео мы не воспроизводим (jt89 моно), но и портить нечего.
        // Маска стерео Game Gear: биты 0-3 — правая сторона каналов 0..3,
        // биты 4-7 — левая. Первая же маска включает стерео-путь.
        else if (cmd == 0x4F) {
            gg_mask = d[pos]; pos += 1;
            sn_stereo = true;
            push_sn_att();
        }
        else if (cmd == 0x3F) { drop_gg++; pos += 1; }  // маска второго чипа
        else if (cmd == 0x50 || cmd == 0x30) {
            // 0x30 — вторая сторона T6W28 (Neo Geo Pocket): стерео с
            // раздельной громкостью, а не второй чип. jt89 у нас один,
            // поэтому стороны сводятся по громкой (0 = максимум, 15 =
            // тишина), иначе отведённые вправо голоса пропали бы.
            uint8_t v = d[pos]; pos += 1;
            int side = (cmd == 0x30) ? 1 : 0;
            if ((v & 0x90) == 0x90) {
                int ch = (v >> 5) & 3;
                // У T6W28 стороны пишутся раздельно, у обычного чипа одна
                // и та же громкость идёт в обе.
                if (sn_dual) sn_att[side][ch] = v & 0x0F;
                else { sn_att[0][ch] = v & 0x0F; sn_att[1][ch] = v & 0x0F; }
                if (side == 0 || !sn_dual)
                    cmds.push_back(0xE0000000u | 0x90u | (unsigned)ch << 5 | (v & 0x0F));
                if (sn_stereo) push_sn_att();
            } else if (side == 0) {
                cmds.push_back(0xE0000000u | v);
            }
        }
        // OPL-семейство на OPL3: YM3812/YM3526/YMF262 п0 — банк 0, YMF262 п1 — банк 1
        else if (cmd == 0x5A || cmd == 0x5B || cmd == 0x5E) {
            cmds.push_back(0xC0000000u | d[pos] << 8 | d[pos+1]); pos += 2;
        }
        else if (cmd == 0x5F) {
            cmds.push_back(0xC0000000u | 0x10000u | d[pos] << 8 | d[pos+1]); pos += 2;
        }
        // OPLL (YM2413/VRC7): 0x51 рег знач -> OP_EXT|EXT_OPLL, транслятор в OPL2
        else if (cmd == 0x51) {
            if (opll) cmds.push_back(0xF0000000u | 0x0A000000u | d[pos] << 8 | d[pos+1]);
            pos += 2;
        }
        // SCC (K051649): 0xD2 порт рег знач -> OP_EXT|EXT_SCC
        else if (cmd == 0xD2) {
            // второй экземпляр SCC — бит 7 байта порта
            if (d[pos] & 0x80) drop2++;
            else cmds.push_back(0xF0000000u | d[pos] << 16 | d[pos+1] << 8 | d[pos+2]);
            pos += 3;
        }
        // K053260: 0xBA рег знач -> OP_EXT|EXT_K060
        else if (cmd == 0xBA) {
            if (d[pos] & 0x80) drop2++;
            else cmds.push_back(0xF2000000u | d[pos] << 8 | d[pos+1]);
            pos += 2;
        }
        // HuC6280: 0xB9 рег знач -> OP_EXT|EXT_HUC
        // PWM Sega 32X: 0xB2 ad dd -> регистр a, значение 12 бит.
        // Разбор сверен с libvgm (Cmd_Ofs4_Data12); бит 7 первого байта —
        // второй экземпляр чипа.
        else if (cmd == 0xB2) {
            if (d[pos] & 0x80) drop2++;
            else cmds.push_back(0xFB000000u | (uint32_t)((d[pos] >> 4) & 7) << 12
                                | (uint32_t)(d[pos] & 0x0F) << 8 | d[pos+1]);
            pos += 2;
        }
        else if (cmd == 0xB1) {
            // RF5C164: регистры чипа
            if (d[pos] & 0x80) drop2++;
            else {
                if ((d[pos] & 0x7F) == 0x07 && !(d[pos+1] & 0x40)) rf5c_wbank = d[pos+1] & 0x0F;
                cmds.push_back(0xF7000000u | (d[pos] & 0xF) << 8 | d[pos+1]); rf5c_regs++;
            }
            pos += 2;
        }
        else if (cmd == 0xC1 || cmd == 0xC2) {
            // Байт в ОЗУ сэмплов RF5C164. Указатель шлём только на разрыве:
            // рипы пишут подряд, и пара на каждый байт удвоила бы очередь.
            uint32_t off = d[pos] | d[pos+1] << 8;
            off = (off & 0x0FFF) | rf5c_wbank << 12;
            if (off != rf5c_ptr) cmds.push_back(0xF8000000u | off);
            cmds.push_back(0xF9000000u | d[pos+2]); rf5c_ram++;
            rf5c_ptr = (uint16_t)(off + 1);
            pos += 3;
        }
        else if (cmd == 0xB9) {
            if (d[pos] & 0x80) drop2++;
            else cmds.push_back(0xF3000000u | (d[pos] & 0xF) << 8 | d[pos+1]);
            pos += 2;
        }
        // OPN (YM2203 0x55, YM2608 0x56/0x57): низ порта 0 — SSG, от $20 — FM
        else if (cmd == 0x55 || cmd == 0x56 || cmd == 0x57) {
            uint32_t port = (cmd == 0x57) ? 1 : 0;
            uint8_t a = d[pos], v = d[pos+1];
            if (port == 0 && a < 0x10) cmds.push_back(0x20000000u | (a & 0xF) << 8 | v);
            else if (a >= 0x20 && !(port == 0 && a >= 0x2D && a <= 0x2F))
                cmds.push_back(0xD0000000u | port << 16 | a << 8 | v);
            pos += 2;
        }
        else if ((cmd & 0xF0) == 0x80) {
            uint8_t b = dac_ptr < dac_bank.size() ? dac_bank[dac_ptr] : 0;
            dac_ptr++;
            cmds.push_back(0xD0000000u | 0x2A00u | b);
            wait_ticks(cmd & 0xF);
        }
        else if (cmd == 0xE0) { dac_ptr = rd32(d, pos); pos += 4; }
        // У чипов с коротким регистровым полем бит 7 байта регистра — это
        // признак ВТОРОГО экземпляра, а не часть адреса. Здесь он раньше
        // просто маскировался (& 15), то есть записи второго чипа садились
        // на регистры первого и дрались с ними.
        else if (cmd == 0xA0) {
            if (d[pos] & 0x80) drop2++;
            else cmds.push_back(0x20000000u | (d[pos] & 15) << 8 | d[pos+1]);
            pos += 2;
        }
        else if (cmd == 0xB4) {
            if (d[pos] & 0x80) drop2++;
            else if (d[pos] > 0x1F) {
                // Дисковая приставка Famicom: пересчёт адресов как в
                // libvgm (Cmd_NES_Reg), см. фирмварь.
                uint32_t a = d[pos];
                uint32_t reg = (a == 0x3F) ? 0x23 : ((a & 0xE0) == 0x20 ? (0x80 | (a & 0x1F)) : a);
                cmds.push_back(0xF6000000u | reg << 8 | d[pos+1]);
            }
            else cmds.push_back(0x90000000u | d[pos] << 8 | d[pos+1]);
            pos += 2;
        }
        else if (cmd == 0xC0) {
            uint32_t off = d[pos] | d[pos+1] << 8;
            // второй экземпляр SegaPCM — старший бит 16-битного смещения
            if (off & 0x8000) drop2++;
            else {
                if (off > 0xFF) pcm_masked++;
                cmds.push_back(0x30000000u | (off & 0xFF) << 8 | d[pos+2]);
            }
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
            } else if (kind == 0x01 || kind == 0x02) {
                // Банк сэмплов RF5C: сам по себе он ничего не играет,
                // из него копирует команда 0x68
                rf5c_bank.insert(rf5c_bank.end(), d.begin() + body, d.begin() + body + len);
            } else if ((kind == 0xC0 || kind == 0xC1) && len >= 2) {
                // Дамп ОЗУ RF5C164: смещение 16 бит, дальше тело
                uint32_t a = (d[body] | d[body+1] << 8) | rf5c_wbank << 12;
                cmds.push_back(0xF8000000u | (a & 0xFFFF));
                for (uint32_t i = 2; i < len; i++) {
                    cmds.push_back(0xF9000000u | d[body + i]); rf5c_ram++;
                }
                rf5c_ptr = (uint16_t)(a + len - 2);
            } else if (kind == 0xC2 && len >= 2) {
                // DPCM-страница NES: [u16 адрес][данные] — синхронно с потоком
                uint32_t a = (d[body] | d[body+1] << 8) & 0x7FFF;
                cmds.push_back(0xA0000000u | a);
                for (uint32_t i = 2; i < len; i++) cmds.push_back(0xB0000000u | d[body + i]);
            }
            pos += 6 + len;
        }
        else if (cmd == 0xB7) {
            if (d[pos] & 0x80) drop2++;  // бит 7 — второй экземпляр, не адрес
            else cmds.push_back(0x40000000u | (d[pos] & 3) << 8 | d[pos+1]);
            pos += 2;
        }
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
        else if (cmd == 0x68) {
            // PCM RAM write: 0x66, тип, источник 24 бита, приёмник 24, длина 24.
            // Рипы Mega CD грузят сэмплы ТОЛЬКО так — раньше команда молча
            // пропускалась, и в ОЗУ чипа не приезжало ни байта.
            uint8_t kind = d[pos + 1];
            uint32_t src = d[pos+2] | d[pos+3] << 8 | d[pos+4] << 16;
            uint32_t dst = d[pos+5] | d[pos+6] << 8 | d[pos+7] << 16;
            uint32_t len = d[pos+8] | d[pos+9] << 8 | d[pos+10] << 16;
            if (!len) len = 0x1000000;
            // Эталон вылезающую за банк заливку не обрезает, а игнорирует целиком
            if ((kind == 0x01 || kind == 0x02) && src < rf5c_bank.size()
                && len <= rf5c_bank.size() - src) {
                dst |= rf5c_wbank << 12;
                cmds.push_back(0xF8000000u | (dst & 0xFFFF));
                for (uint32_t i = 0; i < len; i++) {
                    cmds.push_back(0xF9000000u | rf5c_bank[src + i]); rf5c_ram++;
                }
                rf5c_ptr = (uint16_t)(dst + len);
            }
            pos += 11;
        }
        else if (cmd >= 0x90 && cmd <= 0x95) { static const int L[] = {4,4,5,10,1,4}; pos += L[cmd-0x90]; }
        // 0xA1-0xAF — зеркало 0x51-0x5F для ВТОРОГО экземпляра FM-чипа
        // (0xA2/0xA3 — второй YM2612 и т.д.). Регистровое поле у них
        // полные 8 бит, поэтому битом 7 второй чип не выбрать, и
        // спецификация отвела отдельные команды. Раньше вся полоса молча
        // проваливалась сюда же, в «пропустить и забыть».
        else if (cmd >= 0xA1 && cmd <= 0xAF) { drop2++; pos += 2; }
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
    if (drop2) fprintf(stderr, "ВНИМАНИЕ: %zu записей ко ВТОРОМУ экземпляру чипа отброшено "
                               "(второго экземпляра в железе нет)\n", drop2);
    if (rf5c_regs || rf5c_ram)
        fprintf(stderr, "RF5C164: записей в регистры %zu, байт в ОЗУ %zu\n", rf5c_regs, rf5c_ram);
    if (drop_gg) fprintf(stderr, "ВНИМАНИЕ: %zu масок стерео Game Gear отброшено "
                                 "(стерео PSG не воспроизводится)\n", drop_gg);

    Tb tb;
    // фазовые инкременты cen: Fchip / CLK_HZ * 2^32
    if (ym_clk) tb.wb(3, true, (uint32_t)((double)ym_clk / CLK_HZ * 4294967296.0 + 0.5));
    if (ay_clk) tb.wb(4, true, (uint32_t)((double)ay_clk / CLK_HZ * 4294967296.0 + 0.5));
    if (pcm_clk) {
        double inc = (double)pcm_clk * 2.0 / CLK_HZ * 4294967296.0;
        tb.wb(5, true, inc >= 4294967295.0 ? 0xFFFFFFFFu : (uint32_t)(inc + 0.5));
    }
    // Глушим всё до того, как расставим гейны этого файла: иначе чип,
    // которого в файле нет, подмешивает свой холостой уровень.
    mute_all(tb);
    // Гейны: неиспользуемые чипы глушим (idle-DC/шум не попадает в микс);
    // SegaPCM 34/64 — баланс Out Run по MAME (0.30 FM / 0.70 PCM)
    // Номера чипов по спецификации VGM; у составных OPN парная часть
    // (SSG) — тот же номер с битом 7, и рипы задают баланс именно там.
    uint8_t fm_id = fm_clk ? 0x02 : (ym2608_clk ? 0x07 : 0x06);
    uint8_t ssg_id = ym2608_clk ? 0x87 : (ym2203_clk ? 0x86 : 0x12);
    uint8_t opl_id = ymf262_clk ? 0x0C : (ym3526_clk ? 0x0A : (opll ? 0x01 : 0x09));
    uint32_t opl_base = opll ? (opll_gain_opt ? opll_gain_opt : 11u) : 16u;   // OPLL см. фирмварь
    tb.wb(6, true, gain_of(adpcm_clk ? 64u : 0u, 0x17) << 24
                 | gain_of(pcm_clk ? 34u : 0u, 0x04) << 16
                 // Отдельный AY 64, SSG внутри OPN 47: отношение FM к SSG
                 // сводится по эталону, см. подробный разбор в фирмвари.
                 | gain_of(ay_clk ? 64u : (opn_clk ? 47u : 0u), ssg_id) << 8
                 | gain_of(ym_clk ? 64u : 0u, 0x03));
    tb.wb(0xC, true, gain_of(opl_clk ? opl_base : 0u, opl_id) << 24
                   | gain_of(nes_clk ? (apu_gain_opt ? apu_gain_opt : 80u) : 0u, 0x14)
                   | gain_of(gb_clk_hdr ? 46u : 0u, 0x13) << 8);   // уровень см. в фирмвари
    // Выходной ФНЧ Mega Drive: только для файлов с YM2612 — у OPN-рипов
    // тот же jt12, но фильтра приставки в тракте нет. 0 = Model 1
    tb.wb(0x2C, true, fm_clk ? 0u : 3u);
    tb.wb(0x2D, true, nes_clk ? nes_flt_opt : 3u);
    tb.wb(0x30, true, mono_opt);
    // RF5C164 (Mega CD) и родич RF5C68: отсчёт раз в 384 такта тактовой
    if (rf5c_clk)
        tb.wb(0x33, true, (uint32_t)((double)(rf5c_clk / 384) / CLK_HZ * 4294967296.0 + 0.5));
    // Гейн 255, а не 64: модуль делит сумму восьми каналов на четыре, чтобы
    // при всех громких каналах не упереться в потолок 16 бит. Эталон такого
    // деления не делает, и без компенсации мы играли ровно на 12 дБ тише.
    tb.wb(0x32, true, rf5c_clk ? 255u : 0u);
    tb.wb(0x37, true, pwm_clk ? (uint32_t)pwm_gain_opt : 0u);
    // Гейн дисковой приставки: старший бит поля NES APU. Значение
    // откалибровано по отношению к APU против эталона, см. фирмварь.
    tb.wb(0x31, true, (rd32(d, 0x84) & 0x80000000u) ? 46u : 0u);
    if (opl_clk) tb.wb(0x14, true, (uint32_t)((double)opl_clk / CLK_HZ * 4294967296.0 + 0.5));
    tb.wb(0x34, true, (opll && vrc7) ? 1u : 0u);   // набор патчей OPLL: 1 = VRC7
    if (scc_clk) {
        // Заголовок VGM несёт половину шинной частоты MSX: у эталона
        // (libvgm, k051649.c) шаг считается от clock*2, и нота выходит
        // f = clock/(16*(N+1)) — вдвое выше расхожей формулы с 32. Чипу
        // отдаём полную частоту, иначе весь SCC играет октавой ниже.
        tb.wb(0x21, true, (uint32_t)((double)scc_clk * 2 / CLK_HZ * 4294967296.0 + 0.5));
        tb.wb(0x22, true, gain_of(64, 0x19));
    }
    if (k060_clk) {
        tb.wb(0x25, true, (uint32_t)((double)k060_clk / CLK_HZ * 4294967296.0 + 0.5));
        tb.wb(0x26, true, gain_of(64, 0x1D));
    }
    if (huc_clk) {
        tb.wb(0x27, true, (uint32_t)((double)huc_clk / CLK_HZ * 4294967296.0 + 0.5));
        tb.wb(0x28, true, gain_of(huc_gain_opt ? huc_gain_opt : 150u, 0x1B));   // уровень см. в фирмвари
    }
    if (opn_clk) {
        // Делители сняты с эталона (libvgm, fmopn.c): FM идёт с частотой
        // clock/(72*pre), SSG получает clock*2/(4*pre), где pre = 1 у
        // YM2203 и 2 у YM2608. Наш jt12 делит по-YM2612, на 144, поэтому
        // в него уходит clock*2/pre. Раньше в FM шёл мастер-клок, а в SSG
        // его четверть всегда — у YM2203 обе части играли октавой ниже.
        uint32_t pre = ym2608_clk ? 2 : 1;
        tb.wb(0x16, true, (uint32_t)((double)opn_clk * 2 / pre / CLK_HZ * 4294967296.0 + 0.5));
        tb.wb(4, true, (uint32_t)((double)opn_clk / (2 * pre) / CLK_HZ * 4294967296.0 + 0.5));
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
    if (sn_clk) {
        tb.wb(0x17, true, (uint32_t)((double)sn_clk / CLK_HZ * 4294967296.0 + 0.5));
        // Разновидность шума из заголовка (поля 0x28 и 0x2A); ноль в поле
        // означает вариант Master System, как было зашито в jt89.
        uint32_t fb = hdr_end > 0x29 ? (d[0x28] | d[0x29] << 8) : 0;
        if (!fb) fb = 0x0009;
        uint32_t w = (hdr_end > 0x2A && d[0x2A]) ? d[0x2A] : 16;
        tb.wb(0x2E, true, (w == 15 ? 1u : 0u) << 16 | fb);
    }
    // В файлах Mega Drive 36 и 207 вместо 33 и 239 — см. фирмварь
    tb.wb(0x15, true, gain_of(sn_clk ? (fm_clk ? 36u : 33u) : 0u, 0x00) << 8
                    | gain_of(fm_clk ? 207u : (opn_clk ? 204u : 0u), fm_id));
    tb.wb(2, true, 1);                       // сброс чипа (чистит FIFO!)
    // разблокировка регистров звука SCC (BR2=0x3F) — только после сброса
    if (scc_clk) tb.wb(0, true, 0xF0000000u | (7u << 16));
    for (int i = 0; i < 2048; i++) tb.step(); // дать сбросу пройти
    // Game Boy в VGM: бит 8 выводит APU из сброса, не поднимая SM83.
    // Только ПОСЛЕ сброса — он пишет тот же регистр и обнулил бы бит.
    tb.wb(0x2F, true, sn_stereo ? 1u : 0u);   // стерео SN76489
    if (gb_clk_hdr) tb.wb(2, true, 1u << 8);

    // Game Boy: привести APU в состояние после загрузчика приставки.
    // Запись в звуковые регистры игнорируется, пока не поднят бит 7 NR52,
    // а на настоящем Game Boy звук включает загрузчик — рип, который сам
    // NR52 не пишет, на приставке играет, а у нас молчал. Значения те же,
    // что в фирмвари: NR52 = 0x80, NR50 = 0x77, NR51 = 0xF3.
    // NR52 обязан идти ПЕРВЫМ: пока питание не поднято, остальные записи
    // отбрасываются, и вставка в обратном порядке всё бы обесценила.
    if (gb_clk_hdr) {
        static const uint32_t gb_boot[3][2] = {{0x26, 0x80}, {0x24, 0x77}, {0x25, 0xF3}};
        std::vector<uint32_t> pre;
        for (auto& rv : gb_boot) pre.push_back(0xF4000000u | rv[0] << 8 | rv[1]);
        cmds.insert(cmds.begin(), pre.begin(), pre.end());
    }

    // NES: то же и по той же причине. $4015 разрешает каналы и по
    // включению нулевой; у части рипов запись осталась за кадром лога, и
    // такие файлы молчали целиком. Эталон делает ровно это (libvgm,
    // np_nes_apu.c: UNMUTE_ON_RESET пишет 0x0F и включена по умолчанию).
    if (nes_clk) {
        cmds.insert(cmds.begin(), 0x90000000u | 0x15u << 8 | 0x0Fu);
    }

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
    // Счётчик выборок DMC: отличает «канал молчит, потому что нечего
    // играть» от «канал просит данные, а они не приходят»
    if (nes_clk) fprintf(stderr, "DMC: выборок из памяти %u\n", tb.wb(0x19, false) >> 16);
    if (rf5c_clk) {
        uint32_t v = tb.wb(0x22, false), r = tb.wb(0x23, false);
        fprintf(stderr, "RF5C164 в чипе: регистров %u, байт в ОЗУ %u, чтений памяти %u\n",
                r & 0xFFFF, v & 0xFFFF, v >> 16);
        fprintf(stderr, "RF5C164 пик выхода чипа: %u (гейн %u)\n", r >> 16, 255u);
    }
    fprintf(stderr, "готово: %zu сэмплов @ %u Гц → %s\n", tb.pcm.size() / 2, rate, out);
    return 0;
}

static void write_wav_file(const char* out, const std::vector<int16_t>& pcm_in, uint32_t rate_in) {
    // Приведение к частоте i2s — см. комментарий у to_out_rate
    std::vector<int16_t> conv;
    const std::vector<int16_t>* src = &pcm_in;
    uint32_t rate = rate_in;
    if (out_stage && rate_in != OUT_RATE && !pcm_in.empty()) {
        conv = to_out_rate(pcm_in, rate_in);
        src = &conv;
        rate = OUT_RATE;
        fprintf(stderr, "выходной каскад: строб стенда %u Гц -> i2s %u Гц "
                "(выборка с удержанием, как в audio.sv)\n", rate_in, OUT_RATE);
    }
    const std::vector<int16_t>& pcm = *src;
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
