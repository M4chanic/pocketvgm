OPL := ../../rtl/vendor/opl3/opl3_pkg.sv $(filter-out ../../rtl/vendor/opl3/opl3_pkg.sv,$(wildcard ../../rtl/vendor/opl3/*.sv)) ../../rtl/vendor/opl3/afifo.v
opl3_tb: main.cpp $(OPL)
	verilator --cc --exe --build -O2 -Wno-fatal --top-module opl3 $(OPL) main.cpp -o ../$@
clean:
	rm -rf obj_dir opl3_tb
.PHONY: clean
