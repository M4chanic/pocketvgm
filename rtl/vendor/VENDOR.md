# Vendored RTL

Code copied from upstream (`.git` files removed). Licenses are in the module
directories.

| Module | Upstream | Commit | License / local patches |
|---|---|---|---|
| jt51 (YM2151) | https://github.com/jotego/jt51 | 4a47f666b67b52b9016f390bcfe3255da0128762 | GPL-3.0 |
| jt49 (AY-3-8910/YM2149) | https://github.com/jotego/jt49 | 7f6abfd08a2af9a92dbd5b32c71ea773248a77e2 | GPL-3.0 |
| openfpga-litex (base core) | https://github.com/agg23/openfpga-litex | c21570d85260856c10f7b11c9e4d233c0fe65748 | MIT |
| apu.sv + regs_savestates.sv (NES APU) | https://github.com/MiSTer-devel/NES_MiSTer | 7f598210af8efb09237818d8a4d03e402daf9705 | GPL-3.0; 2 Verilator fixes (inlined constants, split dma_address) |
| verilog-6502 (Arlet Ottens) | https://github.com/Arlet/verilog-6502 | e930327ffecc5062bfce70bbcfba2bbfa6de6e4c | permissive (keep copyright) |
| VerilogBoy (SM83 + GB APU) | https://github.com/zephray/VerilogBoy (refactor branch) | ba256042fbd3274090df86828d84d09527559113 | OHDL 1.0; patch: module cpu renamed to vb_cpu (clash with Arlet) |
| SID (sid_top et al.) | https://github.com/MiSTer-devel/C64_MiSTer (rtl/sid; DAC/filters based on Dag Lem's reDIP-SID/reSID) | 95d3a23867ed52e8655ee85eb950cb9b99a1c044 | GPL/CERN-OHL-S; Verilator fixes: wire arrays -> localparam, wire -> input in function ports, wire -> logic for procedural arrays |
| opl3_fpga (YMF262) | https://github.com/gtaylormb/opl3_fpga | 491a4dc9bb25c33e0183caaf683102b9ce273bc1 | LGPL-3.0; m4pocket patches: NUM_BANKS=1 (OPL2 subset for area, opl3_pkg), BANK_WIDTH guard for 1 bank (mem_multi_bank*), two bank-1 hardcodes -> NUM_BANKS-1 (channels.sv, control_operators.sv); earlier: CDC in host_if, CLK_DIV_COUNT=256 instead of $ceil, trick_sw_detection |
| GENMIDI lump (GM patches for OPL) | https://github.com/freedoom/freedoom (lumps/genmidi, mkgenmidi build) | — | BSD-3-Clause |
| jt12 (YM2612/OPN2) | https://github.com/jotego/jt12 | eaab7e1de6594982a299bc9101dc882384b85685 | GPL-3.0 |
| jt89 (SN76489) | https://github.com/jotego/jt89 | b688c5767f4b5910c43f4c2b3909142156a8f584 | GPL-3.0 |
| font8x8 (UI font) | https://github.com/dhepper/font8x8 | — | Public Domain |
| IKASCC (Konami SCC / K051649) | https://github.com/ika-musume/IKASCC | 5d07811350621de275b56f3ff88c439d0973521d | BSD-2-Clause; used with IMPL_TYPE=0 (sync). Files: IKASCC.v + IKASCC_player_s.v + IKASCC_vrc_s.v + IKASCC_primitives.v (the *_a async variants are kept for upstream fidelity but unused) |
