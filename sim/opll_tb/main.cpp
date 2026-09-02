// Стенд транслятора OPLL->OPL2: читает пары "регистр значение" из stdin
// (hex), кормит модуль без пауз и печатает выданные записи OPL2.
#include "Vopll2opl.h"
#include "verilated.h"
#include <cstdio>
#include <vector>
#include <cstring>
#include <cstdlib>
int main(int argc, char** argv) {
    Verilated::commandArgs(argc, argv);
    bool vrc7 = argc > 1 && argv[1][0] == '1'; bool step = argc > 2;
    Vopll2opl t;
    auto tick = [&]() { t.clk = 0; t.eval(); t.clk = 1; t.eval(); };
    t.rst = 1; t.vrc7 = vrc7; t.wr = 0; t.out_ack = 0;
    for (int i = 0; i < 4; i++) tick();
    t.rst = 0;
    // Вход: "reg val" (hex) или "wait N" — пауза проходит на выход как
    // есть (режим -q: без эха входа, выход годится стенду opl3_tb).
    bool quiet = argc > 3;
    unsigned a, d; int n = 0;
    std::vector<std::pair<int,int>> in;   // (-1, N) — пауза
    char line[64];
    while (fgets(line, sizeof line, stdin)) {
        if (!strncmp(line, "wait", 4)) in.push_back({-1, atoi(line + 4)});
        else if (sscanf(line, "%x %x", &a, &d) == 2) in.push_back({(int)a, (int)d});
    }
    size_t ip = 0; int idle = 0;
    for (long cyc = 0; cyc < 200000000 && idle < 2000; cyc++) {
        t.wr = 0;
        if (ip < in.size() && !t.full && (!step || !t.busy)) {
            if (in[ip].first < 0) { if (!t.busy) { printf("wait %d\n", in[ip].second); ip++; } }
            else { t.wr = 1; t.addr = in[ip].first; t.data = in[ip].second; ip++;
                   if (!quiet) printf("< %02x %02x\n", in[ip-1].first, in[ip-1].second); }
        }
        t.out_ack = 0;
        if (t.out_valid) { printf(quiet ? "%02x %02x\n" : "  > %02x %02x\n", t.out_reg, t.out_val); t.out_ack = 1; n++; }
        tick();
        idle = (ip >= in.size() && !t.busy) ? idle + 1 : 0;
    }
    fprintf(stderr, "записей OPL2: %d, занято на выходе: %d\n", n, (int)t.busy);
    return 0;
}
