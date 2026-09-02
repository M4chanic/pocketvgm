// Стенд одного ядра opl3_fpga: регистры из stdin ("reg val" hex, строка
// "wait N" — пауза в сэмплах), выход — RMS по окнам 50 мс и raw-сэмплы.
#include "Vopl3.h"
#include "verilated.h"
#include <cstdio>
#include <cstring>
#include <cmath>
#include <vector>
int main(int argc, char** argv) {
    Verilated::commandArgs(argc, argv);
    Vopl3 t;
    long cyc = 0;
    std::vector<int> samples;
    auto tick = [&]() {
        t.clk = 0; t.clk_host = 0; t.clk_dac = 0; t.eval();
        t.clk = 1; t.clk_host = 1; t.clk_dac = 1; t.eval();
        if (t.sample_valid) samples.push_back((int)(int32_t)(t.sample_l << 8) >> 8);
        cyc++;
    };
    t.ic_n = 0; t.cs_n = 1; t.wr_n = 1; t.rd_n = 1; t.address = 0; t.din = 0;
    for (int i = 0; i < 64; i++) tick();
    t.ic_n = 1;
    for (int i = 0; i < 4096; i++) tick();
    auto wr = [&](int port, int v) {
        t.address = port; t.din = v; t.cs_n = 0; t.wr_n = 0;
        for (int i = 0; i < 3; i++) tick();
        t.cs_n = 1; t.wr_n = 1;
        for (int i = 0; i < 6; i++) tick();
    };
    char line[64];
    while (fgets(line, sizeof line, stdin)) {
        unsigned a, d;
        if (!strncmp(line, "wait", 4)) { long n = atol(line + 4); size_t target = samples.size() + n; while (samples.size() < target) tick(); }
        else if (sscanf(line, "%x %x", &a, &d) == 2) { wr(0, a); wr(1, d); }
    }
    const int win = 49716 / 20;
    for (size_t i = 0; i + win <= samples.size(); i += win) {
        double s = 0; for (int k = 0; k < win; k++) s += (double)samples[i+k] * samples[i+k];
        printf("%.0f ", sqrt(s / win));
    }
    printf("\nсэмплов %zu\n", samples.size());
    if (argc > 1) { FILE* f = fopen(argv[1], "wb"); for (int v : samples) { short s = v; fwrite(&s, 2, 1, f); } fclose(f); }
    return 0;
}
