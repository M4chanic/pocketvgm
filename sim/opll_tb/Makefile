opll_tb: main.cpp ../../rtl/chipbox/opll2opl.sv
	verilator --cc --exe --build -O2 -Wno-fatal --top-module opll2opl ../../rtl/chipbox/opll2opl.sv main.cpp -o ../$@
clean:
	rm -rf obj_dir opll_tb
.PHONY: clean
