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


def make_noise(nfreq=0x10, seconds=4):
    """Только шумовой канал 4 — для проверки шумовой части отдельно."""
    body = bytearray()
    body += huc(0, 4)            # канал 4 (первый с шумом)
    body += huc(1, 0xFF)         # общая громкость
    body += huc(5, 0xFF)         # баланс
    body += huc(7, 0x80 | (nfreq & 0x1F))  # шум включён, частота
    body += huc(4, 0x9F)         # канал включён, громкость 31
    body += WAIT_60 * (60 * seconds)
    body += b"\x66"
    hdr = bytearray(0x100)
    hdr[0x00:0x04] = b"Vgm "
    hdr[0x08:0x0C] = struct.pack("<I", 0x161)
    hdr[0x34:0x38] = struct.pack("<I", 0x100 - 0x34)
    hdr[0xA4:0xA8] = struct.pack("<I", CLK)
    hdr[0x18:0x1C] = struct.pack("<I", 60 * 60 * seconds)
    out = bytes(hdr) + bytes(body)
    return out[:4] + struct.pack("<I", len(out) - 4) + out[8:]


def make_dda(rate_div=4, seconds=4):
    """Режим прямого вывода: поток отсчётов с известной частотой.

    Ударные на этом чипе играются именно так, и именно на них слышен шум.
    Ни один прежний изолированный тест DDA не затрагивал.

    Пишем синус из 32 точек, между записями пауза rate_div отсчётов при
    44100 Гц: частота тона = 44100 / (rate_div * 32).
    """
    import math as _m
    body = bytearray()
    body += huc(0, 0)                 # канал 0
    body += huc(1, 0xFF)              # общая громкость
    body += huc(5, 0xFF)              # баланс
    body += huc(4, 0xDF)              # включён + DDA (бит 6), громкость 31
    n = int(44100 / rate_div * seconds)
    wait = bytes([0x70 | (rate_div - 1)])   # 0x7n = пауза n+1 отсчётов
    for i in range(n):
        v = int(round(15.5 + 15.0 * _m.sin(2 * _m.pi * (i % 32) / 32))) & 0x1F
        body += huc(6, v) + wait
    body += b"\x66"
    hdr = bytearray(0x100)
    hdr[0x00:0x04] = b"Vgm "
    hdr[0x08:0x0C] = struct.pack("<I", 0x161)
    hdr[0x34:0x38] = struct.pack("<I", 0x100 - 0x34)
    hdr[0xA4:0xA8] = struct.pack("<I", CLK)
    hdr[0x18:0x1C] = struct.pack("<I", 44100 * seconds)
    out = bytes(hdr) + bytes(body)
    return out[:4] + struct.pack("<I", len(out) - 4) + out[8:]


def make_bare(period=0x0FE, seconds=4):
    """Канал включён, таблица волны НЕ записана.

    Проверка постоянного смещения: у нас незаполненная таблица даёт
    постоянные -16 на отсчёт, и включение такого канала становится
    ступенью. Нужно увидеть, что в этом случае выдаёт эталон.
    """
    body = bytearray()
    body += huc(0, 0)
    body += huc(1, 0xFF)
    body += huc(2, period & 0xFF)
    body += huc(3, (period >> 8) & 0x0F)
    body += huc(5, 0xFF)
    body += huc(4, 0x9F)         # включён, громкость 31, волна не писалась
    body += WAIT_60 * (60 * seconds)
    body += b"\x66"
    hdr = bytearray(0x100)
    hdr[0x00:0x04] = b"Vgm "
    hdr[0x08:0x0C] = struct.pack("<I", 0x161)
    hdr[0x34:0x38] = struct.pack("<I", 0x100 - 0x34)
    hdr[0xA4:0xA8] = struct.pack("<I", CLK)
    hdr[0x18:0x1C] = struct.pack("<I", 60 * 60 * seconds)
    out = bytes(hdr) + bytes(body)
    return out[:4] + struct.pack("<I", len(out) - 4) + out[8:]


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
    if len(sys.argv) > 1 and sys.argv[1] == "--dda":
        path = sys.argv[2] if len(sys.argv) > 2 else "huc_dda.vgm"
        open(path, "wb").write(make_dda())
        print(f"{path}: прямой вывод, ожидаемый тон {44100 / (4 * 32):.1f} Гц")
    elif len(sys.argv) > 1 and sys.argv[1] == "--bare":
        path = sys.argv[2] if len(sys.argv) > 2 else "huc_bare.vgm"
        open(path, "wb").write(make_bare())
        print(f"{path}: канал включён без записи волновой таблицы")
    elif len(sys.argv) > 1 and sys.argv[1] == "--noise":
        nf = int(sys.argv[2], 0) if len(sys.argv) > 2 else 0x10
        path = sys.argv[3] if len(sys.argv) > 3 else "huc_noise.vgm"
        open(path, "wb").write(make_noise(nf))
        print(f"{path}: шум, регистр частоты {nf:#04x}")
    else:
        period = int(sys.argv[1], 0) if len(sys.argv) > 1 else 0x0FE
        path = sys.argv[2] if len(sys.argv) > 2 else "huc_test.vgm"
        open(path, "wb").write(make(period))
        print(f"{path}: период {period}, ожидаемая частота "
              f"{CLK / (32 * (period + 1)):.1f} Гц")
