#!/usr/bin/env python3
"""Синтетический VGM для HuC6280: один канал, синус, известная частота.

Нужен, чтобы сравнивать наш чип с эталоном на входе, который мы полностью
контролируем. На музыке видно только «звучит не так», а здесь ожидаемая
частота считается формулой и проверяется числом.

Ожидаемая частота = 3579545 / (32 * (P + 1)), где P — 12-битный период.
"""

import struct
import sys

CLK = 3_579_545
WAIT_60 = b"\x62"  # пауза 1/60 с


def huc(reg, val):
    return bytes([0xB9, reg & 0x0F, val & 0xFF])


def make(period=0x0FE, seconds=4):
    body = bytearray()
    body += huc(0, 0)        # канал 0
    body += huc(1, 0xFF)     # общая громкость
    body += huc(4, 0x00)     # канал выключен: индекс волны с нуля
    import math
    for i in range(32):      # синус, 5 бит без знака
        body += huc(6, int(round(15.5 + 15.0 * math.sin(2 * math.pi * i / 32))) & 0x1F)
    body += huc(2, period & 0xFF)
    body += huc(3, (period >> 8) & 0x0F)
    body += huc(5, 0xFF)     # баланс
    body += huc(4, 0x9F)     # канал включён, громкость 31
    body += WAIT_60 * (60 * seconds)
    body += b"\x66"          # конец

    hdr = bytearray(0x100)
    hdr[0x00:0x04] = b"Vgm "
    hdr[0x08:0x0C] = struct.pack("<I", 0x161)
    hdr[0x34:0x38] = struct.pack("<I", 0x100 - 0x34)   # смещение данных
    hdr[0xA4:0xA8] = struct.pack("<I", CLK)            # клок HuC6280
    hdr[0x18:0x1C] = struct.pack("<I", 60 * 60 * seconds)  # всего сэмплов
    out = bytes(hdr) + bytes(body)
    out = out[:4] + struct.pack("<I", len(out) - 4) + out[8:]
    return out


if __name__ == "__main__":
    period = int(sys.argv[1], 0) if len(sys.argv) > 1 else 0x0FE
    path = sys.argv[2] if len(sys.argv) > 2 else "huc_test.vgm"
    open(path, "wb").write(make(period))
    print(f"{path}: период {period} (={period}), ожидаемая частота "
          f"{CLK / (32 * (period + 1)):.1f} Гц")
