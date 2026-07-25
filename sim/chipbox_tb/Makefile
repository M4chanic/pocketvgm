JT51_HDL := ../../rtl/vendor/jt51/hdl
JT51_SRC := $(addprefix $(JT51_HDL)/,$(shell grep -v '\.\./' $(JT51_HDL)/jt51.f))
JT49_HDL := ../../rtl/vendor/jt49/hdl
JT49_SRC := $(addprefix $(JT49_HDL)/,jt49.v jt49_cen.v jt49_div.v jt49_eg.v jt49_exp.v jt49_noise.v)
JTOUTRUN := ../../rtl/vendor/jtoutrun/jtoutrun_pcm.v ../../rtl/vendor/jtoutrun/jtframe_dual_ram.v
JTOUTRUN += ../../rtl/chipbox/msm6258.sv
NES      := ../../rtl/vendor/nes/apu.sv ../../rtl/chipbox/savestate_stub.sv ../../rtl/chipbox/vrc6.sv
NES      += ../../rtl/vendor/verilog-6502/cpu.v ../../rtl/vendor/verilog-6502/ALU.v
VB_HDL   := ../../rtl/vendor/verilogboy
GB       := $(addprefix $(VB_HDL)/,cpu.v alu.v control.v regfile.v singlereg.v \
            sound.v sound_channel_mix.v sound_length_ctr.v sound_noise.v \
            sound_square.v sound_vol_env.v sound_wave.v edgedet.v)
GB       += ../../rtl/chipbox/gbsbox.sv
HUC      := ../../rtl/chipbox/huc6280_psg.sv
SID      := $(wildcard ../../rtl/vendor/c64sid/sid_*.sv)
GEN      := $(wildcard ../../rtl/vendor/jt12/*.v) $(wildcard ../../rtl/vendor/jt89/*.v)
OPL      := ../../rtl/vendor/opl3/opl3_pkg.sv $(filter-out ../../rtl/vendor/opl3/opl3_pkg.sv,$(wildcard ../../rtl/vendor/opl3/*.sv)) ../../rtl/vendor/opl3/afifo.v
SCC      := ../../rtl/vendor/scc/IKASCC.v ../../rtl/vendor/scc/IKASCC_player_s.v ../../rtl/vendor/scc/IKASCC_vrc_s.v ../../rtl/vendor/scc/IKASCC_primitives.v
OKIM     := $(wildcard ../../rtl/vendor/jt6295/*.v)
K060     := $(wildcard ../../rtl/vendor/jt053260/*.v)
CHIPBOX  := ../../core/target/pocket/chipbox.sv

chipbox_tb: main.cpp $(CHIPBOX) $(JT51_SRC) $(JT49_SRC) $(JTOUTRUN) $(NES) $(GB)
	verilator --cc --exe --build -O3 -Wno-fatal --top-module chipbox \
		-GCLK_HZ=8000000 -DM4_SIM -I$(VB_HDL) \
		$(NES) $(GB) $(SID) $(OPL) $(SCC) $(HUC) $(OKIM) $(K060) $(GEN) $(CHIPBOX) $(JT51_SRC) $(JT49_SRC) $(JTOUTRUN) main.cpp -CFLAGS "-O2" -LDFLAGS "-lz" -o ../$@

# Verilator собирается с -Wno-fatal — иначе не проходит вендорный код.
# Из-за этого предупреждения в НАШИХ файлах молча терялись: два драйвера
# на huc_wr дошли до Quartus и уронили сборку через сорок минут. Здесь
# те же предупреждения, но по своим путям и только классы, означающие
# настоящую ошибку синтеза: два драйвера, смесь блокирующих присваиваний,
# неявные провода, защёлки, выход за границы массива. Ширины операндов
# сюда не входят — их в проекте давний безобидный шум.
lint:
	@python3 ../../scripts/check_multidriver.py
	@verilator --lint-only -Wno-fatal --top-module chipbox \
		-GCLK_HZ=8000000 -DM4_SIM -I$(VB_HDL) \
		$(NES) $(GB) $(SID) $(OPL) $(SCC) $(HUC) $(OKIM) $(K060) $(GEN) \
		$(CHIPBOX) $(JT51_SRC) $(JT49_SRC) $(JTOUTRUN) 2>&1 \
		| grep -E "rtl/chipbox|core/target/pocket" \
		| grep -E "MULTIDRIVEN|BLKANDNBLK|IMPLICIT|LATCH|SELRANGE" > /tmp/m4lint.txt || true
	@if [ -s /tmp/m4lint.txt ]; then \
		echo "линт: предупреждения в собственных файлах:"; cat /tmp/m4lint.txt; exit 1; \
	else echo "линт: собственные файлы чисты"; fi

clean:
	rm -rf obj_dir chipbox_tb

.PHONY: clean lint
