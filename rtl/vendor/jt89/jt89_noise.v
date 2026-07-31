/*  This file is part of JT89.

    JT89 is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    JT89 is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with JT89.  If not, see <http://www.gnu.org/licenses/>.

    Author: Jose Tejada Gomez. Twitter: @topapate
    Version: 1.0
    Date: March, 8th 2017

    This work was originally based in the implementation found on the
    SMS core of MiST

    */

// m4pocket: отводы и ширина сдвигового регистра сделаны настраиваемыми.
//
// Здесь была зашита разновидность Master System: 16 бит, отводы 0 и 3. У
// SN76489AN (SG-1000, аркады, BBC Micro) регистр 15-битный, а отводы 0 и
// 1 — шум звучит иначе и по высоте, и по окраске. Заголовок VGM несёт
// эти параметры в полях 0x28 (маска отводов) и 0x2A (ширина), и раньше мы
// их не читали вовсе.
//
// Значения по умолчанию воспроизводят прежнее поведение бит в бит:
// fb_mask = 16'h0009 даёт ^(shift & 9) = shift[0]^shift[3], sr15 = 0.
module jt89_noise(
    input               clk,
(* direct_enable = 1 *) input   clk_en,
    input               rst,
    input               clr,
    input         [2:0] ctrl3,
    input         [3:0] vol,
    input               tone2,
    input        [15:0] fb_mask,
    input               sr15,     // 1 = регистр 15 бит, 0 = 16 бит
    output              raw,      // выход до применения громкости
    output        [8:0] snd
);

reg [15:0] shift;
reg [10:0] cnt;
reg        update, tone_en, tone2_l;
wire       up;

assign up = tone_en ? tone2 & ~tone2_l : cnt==1;

jt89_vol u_vol(
    .rst    ( rst       ),
    .clk    ( clk       ),
    .clk_en ( clk_en    ),
    .din    ( shift[0]  ),
    .vol    ( vol       ),
    .snd    ( snd       )
);

always @(posedge clk)
    if( rst ) begin
        cnt     <= 0;
        tone_en <= 0;
    end else if( clk_en ) begin
        tone_en <= ctrl3[1:0]==3;
        tone2_l <= tone2;

        if( cnt==11'd1 ) begin
            case( ctrl3[1:0] )
                2'd0: cnt <= 11'h20; // clk_en already divides by 16
                2'd1: cnt <= 11'h40;
                2'd2: cnt <= 11'h80;
                default:;
            endcase
        end else begin
            cnt <= cnt-11'b1;
        end
    end

assign raw = shift[0];

// Белый шум складывает отводы по маске, периодический берёт один бит —
// как и было, только маска теперь задаётся снаружи.
wire fb = ctrl3[2] ? ^(shift & fb_mask) : shift[0];

// Стартовое значение и место вдвига зависят от ширины регистра: у 15
// битов старший разряд не используется вовсе.
wire [15:0] seed = sr15 ? {1'b0, 1'b1, 14'd0} : {1'b1, 15'd0};
wire [15:0] next = sr15 ? {1'b0, fb, shift[14:1]} : {fb, shift[15:1]};
wire        empty = sr15 ? (shift[14:0] == 15'd0) : (shift == 16'd0);

always @(posedge clk)
    if( rst || clr )
        shift <= seed;
    else if( clk_en ) begin
        if( up ) begin
            shift <= empty ? seed : next;
        end
    end

endmodule
