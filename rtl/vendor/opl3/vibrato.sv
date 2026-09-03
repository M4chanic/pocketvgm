/*******************************************************************************
#   +html+<pre>
#
#   FILENAME: vibrato.sv
#   AUTHOR: Greg Taylor     CREATION DATE: 13 Oct 2014
#
#   DESCRIPTION:
#   Prepare the phase increment for the NCO (calc multiplier and vibrato)
#
#   CHANGE HISTORY:
#   13 Oct 2014    Greg Taylor
#       Initial version
#
#   Copyright (C) 2014 Greg Taylor <gtaylor@sonic.net>
#
#   This file is part of OPL3 FPGA.
#
#   OPL3 FPGA is free software: you can redistribute it and/or modify
#   it under the terms of the GNU Lesser General Public License as published by
#   the Free Software Foundation, either version 3 of the License, or
#   (at your option) any later version.
#
#   OPL3 FPGA is distributed in the hope that it will be useful,
#   but WITHOUT ANY WARRANTY; without even the implied warranty of
#   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
#   GNU Lesser General Public License for more details.
#
#   You should have received a copy of the GNU Lesser General Public License
#   along with OPL3 FPGA.  If not, see <http://www.gnu.org/licenses/>.
#
#   Original Java Code:
#   Copyright (C) 2008 Robson Cozendey <robson@cozendey.com>
#
#   Original C++ Code:
#   Copyright (C) 2012  Steffen Ohrendorf <steffen.ohrendorf@gmx.de>
#
#   Some code based on forum posts in:
#   http://forums.submarine.org.uk/phpBB/viewforum.php?f=9,
#   Copyright (C) 2010-2013 by carbon14 and opl3
#
#******************************************************************************/
`timescale 1ns / 1ps
`default_nettype none

module vibrato
    import opl3_pkg::*;
(
    input wire clk,
    input wire sample_clk_en,
    input wire [BANK_NUM_WIDTH-1:0] bank_num,
    input wire [OP_NUM_WIDTH-1:0] op_num,
    input wire [REG_FNUM_WIDTH-1:0] fnum,
    input wire dvb,
    output logic signed [VIB_VAL_WIDTH:0] vib_val_p0
);
    // m4pocket: rewritten after Nuked OPL3 (die-verified). Upstream counted
    // the LFO once per operator slot (36x too fast, 109 Hz here with one
    // bank) and added the offset after the block shift and multiplier, so
    // the depth shrank with the octave. The offset now goes to F-number
    // before the shift, from a counter that steps once per sample.
    localparam VIBRATO_INDEX_WIDTH = 13;

    logic [VIBRATO_INDEX_WIDTH-1:0] vibrato_index = 0;
    logic [2:0] pos;
    logic [VIB_VAL_WIDTH-1:0] range0, range1, range2;

    /*
     * Low-Frequency Oscillator (LFO)
     * 6.07Hz (Sample Freq/2**13): one count per sample
     */
    always_ff @(posedge clk)
        if (sample_clk_en && bank_num == 0 && op_num == 0)
            vibrato_index <= vibrato_index + 1;

    always_comb begin
        pos = vibrato_index[12:10];
        range0 = fnum >> 7;
        range1 = (pos & 3) == 0 ? '0 : (pos & 1) ? range0 >> 1 : range0;
        range2 = dvb ? range1 : range1 >> 1;
        vib_val_p0 = pos[2] ? -$signed({1'b0, range2}) : $signed({1'b0, range2});
    end
endmodule
`default_nettype wire
