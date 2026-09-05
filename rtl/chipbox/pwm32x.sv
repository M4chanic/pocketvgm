// PWM Sega 32X — ЦАП с широтно-импульсной модуляцией.
//
// Своя реализация по эталону: libvgm, emu/cores/pwm.c (ядро Gens/GS,
// GPL-2+). Чип простой: программа SH-2 пишет ширину импульса, а уровень
// на выходе — эта ширина относительно середины периода. Никаких
// генераторов и огибающих, вся «музыка» лежит в потоке записей.
//
// Регистры (в VGM это команда 0xB2, номер регистра в старшем полубайте):
//   0 управление (время прерывания — на звук не влияет)
//   1 период (cycle): offset = (cycle-1 & 0xFFF)/2 + 1, scale = 0x7FFF00/offset
//   2 левый канал, 3 правый, 4 оба сразу
//
// Уровень канала: значение 0 даёт ровный ноль, иначе 12 бит трактуются
// как знаковые, из них вычитается offset и результат умножается на
// scale со сдвигом на 8. Ноль-как-тишина и знаковая трактовка — это
// поправки Chilly Willy из эталона: без них Knuckles' Chaotix щёлкает
// (значения 0xF.. там означают отрицательные), а старт и пауза трека
// дают хлопки.
//
// Хак с первой записью: если первое значение правого канала совпало с
// левым (или пришло в регистр «оба»), эталон запоминает его как offset —
// это убирает постоянную составляющую конкретного рипа. Повторно offset
// уже не трогается, и scale от этого НЕ пересчитывается.
module pwm32x (
    input wire clk,
    input wire rst,

    input wire wr,            // строб записи, один такт
    input wire [2:0] sel,     // номер регистра 0..4
    input wire [11:0] din,    // значение

    output wire signed [15:0] snd_l,
    output wire signed [15:0] snd_r
);

  reg [11:0] out_l = 0;
  reg [11:0] out_r = 0;
  reg signed [13:0] offset = 14'sd2048;  // сброс: cycle 0 -> 0xFFF -> 0x800
  reg [23:0] scale = 24'd4095;
  reg mode = 0;  // offset уже подобран по первой записи

  // Делитель для scale = 0x7FFF00 / offset. Запись периода — редкость
  // (раз в трек), поэтому последовательного деления за 24 такта хватает
  // с запасом, а таблица на 2048 значений стоила бы пяти блоков памяти.
  reg [23:0] div_rem = 0;
  reg [23:0] div_quo = 0;
  reg [13:0] div_dsr = 1;
  reg [4:0] div_cnt = 0;
  wire div_busy = div_cnt != 0;
  wire [24:0] div_sub = {div_rem[22:0], div_quo[23]} - {11'b0, div_dsr};

  // Следующий offset по записи периода: (din-1)>>1 + 1, 13 бит
  wire [11:0] cyc_next = din - 12'd1;
  wire [12:0] off_next = {2'b0, cyc_next[11:1]} + 13'd1;

  always @(posedge clk) begin
    if (rst) begin
      out_l <= 0;
      out_r <= 0;
      offset <= 14'sd2048;
      scale <= 24'd4095;
      mode <= 0;
      div_cnt <= 0;
    end else begin
      if (div_busy) begin
        if (div_sub[24]) begin
          // не вычиталось: разряд частного 0
          div_rem <= {div_rem[22:0], div_quo[23]};
          div_quo <= {div_quo[22:0], 1'b0};
        end else begin
          div_rem <= div_sub[23:0];
          div_quo <= {div_quo[22:0], 1'b1};
        end
        div_cnt <= div_cnt - 5'd1;
        if (div_cnt == 5'd1) scale <= div_sub[24] ? {div_quo[22:0], 1'b0} : {div_quo[22:0], 1'b1};
      end
      if (wr) begin
        case (sel)
          3'd1: begin
            // период: offset = ((din-1) & 0xFFF)/2 + 1. Ширины считаем
            // явно: неявное усечение внутри выражения уже стоило нам
            // одного чипа (см. rf5c164 и verilog-width-trap).
            offset <= $signed({1'b0, off_next});
            div_rem <= 0;
            div_quo <= 24'h7FFF00;
            div_dsr <= {1'b0, off_next};
            div_cnt <= 5'd24;
          end
          3'd2: out_l <= din;
          3'd3: begin
            out_r <= din;
            if (!mode && out_l == din) begin
              offset <= $signed({2'b0, din});
              mode   <= 1;
            end
          end
          3'd4: begin
            out_l <= din;
            out_r <= din;
            if (!mode) begin
              offset <= $signed({2'b0, din});
              mode   <= 1;
            end
          end
          default: ;  // 0 — управление, на звук не влияет
        endcase
      end
    end
  end

  // Выход: ступенями по такту — вычитание, умножение, насыщение. На
  // 48 кГц это всё равно мгновенно, а одним выражением получается путь
  // на 30 нс (см. историю фильтра NES).
  function automatic signed [13:0] sx12(input [11:0] v);
    sx12 = {{2{v[11]}}, v};
  endfunction

  reg signed [14:0] diff_l = 0, diff_r = 0;
  reg zero_l = 1, zero_r = 1;
  reg signed [39:0] prod_l = 0, prod_r = 0;
  reg zero_l1 = 1, zero_r1 = 1;
  reg signed [15:0] out_l_r = 0, out_r_r = 0;

  wire signed [39:0] shl = prod_l >>> 8;
  wire signed [39:0] shr = prod_r >>> 8;

  always @(posedge clk) begin
    diff_l <= sx12(out_l) - offset;
    diff_r <= sx12(out_r) - offset;
    zero_l <= (out_l == 12'd0);
    zero_r <= (out_r == 12'd0);

    prod_l <= diff_l * $signed({1'b0, scale});
    prod_r <= diff_r * $signed({1'b0, scale});
    zero_l1 <= zero_l;
    zero_r1 <= zero_r;

    out_l_r <= zero_l1 ? 16'sd0
        : (shl > 40'sd32767 ? 16'sd32767 : (shl < -40'sd32768 ? -16'sd32768 : shl[15:0]));
    out_r_r <= zero_r1 ? 16'sd0
        : (shr > 40'sd32767 ? 16'sd32767 : (shr < -40'sd32768 ? -16'sd32768 : shr[15:0]));
  end

  assign snd_l = out_l_r;
  assign snd_r = out_r_r;

endmodule
