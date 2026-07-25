// HuC6280 PSG (PC Engine / TurboGrafx-16) — 6 каналов волновой таблицы.
//
// Своё, по открытому описанию регистров ($0800-$0809). Готового Verilog
// не нашлось: в ядрах PC Engine эта часть написана на VHDL, а её нам не
// прожевать Verilator'ом, на котором стоит вся проверка проекта.
//
// Каналы обсчитываются по очереди, один за такт clk: волновая память
// одна на все шесть (6 x 32 x 5 бит) и читается по одному отсчёту, иначе
// 960 триггеров ушли бы в логику. Полный проход укладывается в 6 тактов
// clk и происходит по каждому cen, то есть на частоте чипа.
//
// Не реализовано: LFO (регистры $0808/$0809 принимаются и игнорируются).
// В рипах он встречается редко, а стоит заметной логики.
module huc6280_psg (
    input  wire               clk,
    input  wire               cen,      // разрешение такта на частоте чипа
    input  wire               rst,
    input  wire               wr,
    input  wire        [3:0]  addr,     // $0800-$0809 -> 0..9
    input  wire        [7:0]  din,
    output reg  signed [15:0] snd_left  = 0,
    output reg  signed [15:0] snd_right = 0
);

  // ------------------------------------------------------------------
  // Регистры
  reg  [2:0] ch_sel = 0;
  reg  [3:0] main_l = 0, main_r = 0;

  reg [11:0] freq   [0:7];
  reg  [4:0] vol    [0:7];
  reg        ch_on  [0:7];
  reg        ch_dda [0:7];
  reg  [3:0] bal_l  [0:7];
  reg  [3:0] bal_r  [0:7];
  reg  [4:0] dda    [0:7];

  reg [11:0] cnt    [0:7];
  reg  [4:0] step   [0:7];
  reg  [4:0] wr_idx [0:7];

  // шум — только каналы 4 и 5
  reg        noise_en [0:7];
  reg  [4:0] noise_fq [0:7];
  reg [11:0] noise_cnt[0:7];
  reg [17:0] lfsr     [0:7];

  reg [4:0] wave_ram[0:191];

  integer i;

  // ------------------------------------------------------------------
  // Запись регистров
  always @(posedge clk) begin
    if (rst) begin
      ch_sel <= 0;
      main_l <= 0;
      main_r <= 0;
      for (i = 0; i < 6; i = i + 1) begin
        freq[i]   <= 0;
        vol[i]    <= 0;
        ch_on[i]  <= 0;
        ch_dda[i] <= 0;
        bal_l[i]  <= 4'hF;
        bal_r[i]  <= 4'hF;
        dda[i]    <= 0;
        cnt[i]    <= 0;
        step[i]   <= 0;
        wr_idx[i] <= 0;
      end
      for (i = 4; i < 6; i = i + 1) begin
        noise_en[i]  <= 0;
        noise_fq[i]  <= 0;
        noise_cnt[i] <= 0;
        lfsr[i]      <= 18'h1;
      end
    end else if (wr) begin
      case (addr)
        4'h0: ch_sel <= din[2:0];
        4'h1: {main_l, main_r} <= din;
        4'h2: freq[ch_sel][7:0]  <= din;
        4'h3: freq[ch_sel][11:8] <= din[3:0];
        4'h4: begin
          // сброс индекса записи волны в момент выключения канала —
          // так рипы заливают таблицу с начала
          if (ch_on[ch_sel] && !din[7]) wr_idx[ch_sel] <= 0;
          ch_on[ch_sel]  <= din[7];
          ch_dda[ch_sel] <= din[6];
          vol[ch_sel]    <= din[4:0];
        end
        4'h5: {bal_l[ch_sel], bal_r[ch_sel]} <= din;
        4'h6: begin
          if (ch_dda[ch_sel]) begin
            dda[ch_sel] <= din[4:0];
          end else begin
            wave_ram[{ch_sel, wr_idx[ch_sel]}] <= din[4:0];
            wr_idx[ch_sel] <= wr_idx[ch_sel] + 1'b1;
          end
        end
        4'h7: if (ch_sel >= 3'd4) begin
          noise_en[ch_sel] <= din[7];
          noise_fq[ch_sel] <= din[4:0];
        end
        default: ; // $08/$09 — LFO, не реализован
      endcase
    end
  end

  // ------------------------------------------------------------------
  // Шаг частоты: период 0 читается как 4096
  always @(posedge clk) begin
    if (rst) begin
      // очищено выше
    end else if (cen) begin
      for (i = 0; i < 6; i = i + 1) begin
        if (cnt[i] == 12'd0) begin
          cnt[i]  <= freq[i];
          step[i] <= step[i] + 1'b1;
        end else begin
          cnt[i] <= cnt[i] - 1'b1;
        end
      end
      for (i = 4; i < 6; i = i + 1) begin
        if (noise_cnt[i] == 12'd0) begin
          noise_cnt[i] <= {~noise_fq[i], 6'd0} + 12'd1;
          lfsr[i] <= {lfsr[i][0] ^ lfsr[i][1] ^ lfsr[i][11] ^ lfsr[i][12]
                      ^ lfsr[i][17], lfsr[i][17:1]};
        end else begin
          noise_cnt[i] <= noise_cnt[i] - 1'b1;
        end
      end
    end
  end

  // ------------------------------------------------------------------
  // Таблица громкости: шаг 1.5 дБ, индекс 0 — тишина
  reg [7:0] vol_tab[0:31];
  initial begin
    vol_tab[ 0] = 8'd0;   vol_tab[ 1] = 8'd1;   vol_tab[ 2] = 8'd1;
    vol_tab[ 3] = 8'd1;   vol_tab[ 4] = 8'd2;   vol_tab[ 5] = 8'd2;
    vol_tab[ 6] = 8'd2;   vol_tab[ 7] = 8'd3;   vol_tab[ 8] = 8'd3;
    vol_tab[ 9] = 8'd4;   vol_tab[10] = 8'd5;   vol_tab[11] = 8'd6;
    vol_tab[12] = 8'd7;   vol_tab[13] = 8'd8;   vol_tab[14] = 8'd10;
    vol_tab[15] = 8'd12;  vol_tab[16] = 8'd14;  vol_tab[17] = 8'd17;
    vol_tab[18] = 8'd20;  vol_tab[19] = 8'd24;  vol_tab[20] = 8'd28;
    vol_tab[21] = 8'd34;  vol_tab[22] = 8'd40;  vol_tab[23] = 8'd48;
    vol_tab[24] = 8'd57;  vol_tab[25] = 8'd68;  vol_tab[26] = 8'd80;
    vol_tab[27] = 8'd96;  vol_tab[28] = 8'd114; vol_tab[29] = 8'd135;
    vol_tab[30] = 8'd161; vol_tab[31] = 8'd191;
  end

  // ------------------------------------------------------------------
  // Обход каналов: по одному за такт clk, старт по cen
  reg  [2:0] scan = 3'd6;   // 6 — простой
  reg signed [17:0] acc_l = 0, acc_r = 0;
  reg  [4:0] samp_q = 0;
  reg  [2:0] scan_q = 0;
  reg  [2:0] scan_q2 = 3'd6;
  wire signed [17:0] term_l = $signed(centred) * $signed({1'b0, vol_tab[vl_i]});
  wire signed [17:0] term_r = $signed(centred) * $signed({1'b0, vol_tab[vr_i]});

  wire [5:0] vl_raw = {1'b0, main_l, 1'b0} + {1'b0, bal_l[scan_q], 1'b0}
                    + {1'b0, vol[scan_q]};
  wire [5:0] vr_raw = {1'b0, main_r, 1'b0} + {1'b0, bal_r[scan_q], 1'b0}
                    + {1'b0, vol[scan_q]};
  // индекс — насыщение до 31; выключенный канал молчит
  wire [4:0] vl_i = !ch_on[scan_q] ? 5'd0 : (vl_raw > 6'd31 ? 5'd31 : vl_raw[4:0]);
  wire [4:0] vr_i = !ch_on[scan_q] ? 5'd0 : (vr_raw > 6'd31 ? 5'd31 : vr_raw[4:0]);

  // шум подменяет волну на каналах 4-5
  wire noisy = (scan_q >= 3'd4) && noise_en[scan_q];
  wire [4:0] raw = ch_dda[scan_q] ? dda[scan_q]
                 : noisy ? (lfsr[scan_q][0] ? 5'd31 : 5'd0)
                 : samp_q;
  wire signed [5:0] centred = {1'b0, raw} - 6'sd16;

  always @(posedge clk) begin
    if (rst) begin
      scan <= 3'd6;
      acc_l <= 0;
      acc_r <= 0;
      snd_left <= 0;
      snd_right <= 0;
    end else begin
      if (cen) scan <= 0;
      else if (scan != 3'd6) scan <= scan + 1'b1;

      // конвейер: адрес -> отсчёт -> накопление
      samp_q <= wave_ram[{scan, step[scan]}];
      scan_q <= scan;
      scan_q2 <= scan_q;

      // сумму начинаем нулевым каналом, а не обнулением по cen: иначе
      // обнуление и накопление конфликтовали бы в одном блоке
      if (scan_q == 3'd0) begin
        acc_l <= term_l;
        acc_r <= term_r;
      end else if (scan_q != 3'd6) begin
        acc_l <= acc_l + term_l;
        acc_r <= acc_r + term_r;
      end

      // шестой канал попадает в acc только на следующем такте
      if (scan_q2 == 3'd5) begin
        snd_left  <= acc_l[17:2];
        snd_right <= acc_r[17:2];
      end
    end
  end


endmodule
