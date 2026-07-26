#!/usr/bin/env python3
"""Синтетический GBS: один квадратный канал, известная частота.

Нужен, чтобы проверить канал изолированно. На музыке видно только, что
мелодии не слышно; здесь драйвер не делает ничего, кроме программирования
CH1, и ожидаемая частота считается формулой.

Частота квадратного канала Game Boy = 131072 / (2048 - x), где x — 11-бит
из NR13/NR14.
"""

import struct
import sys

LOAD = 0x0400  # ниже $0400 наше ядро не поддерживает: там стаб


def sm83_init(freq_x, ch=1):
    """LD A,n / LDH (nn),A для каждого регистра, затем RET."""
    base = 0x10 + (ch - 1) * 5  # CH1 -> $FF10, CH2 -> $FF15
    writes = [
        (0x26, 0x80),          # NR52: звук включён
        (0x25, 0xFF),          # NR51: оба выхода
        (0x24, 0x77),          # NR50: полная громкость
        (base + 0, 0x00),      # свип выключен
        (base + 1, 0x80),      # скважность 50%, длительность 0
        (base + 2, 0xF0),      # громкость 15, огибающая выключена
        (base + 3, freq_x & 0xFF),
        (base + 4, 0x80 | ((freq_x >> 8) & 0x07)),  # запуск + старшие биты
    ]
    code = bytearray()
    for reg, val in writes:
        code += bytes([0x3E, val, 0xE0, reg])
    code.append(0xC9)          # RET
    return code


def make(freq_x=0x6D6, ch=1):
    init = sm83_init(freq_x, ch)
    play = bytes([0xC9])       # PLAY ничего не делает
    body = init + play

    h = bytearray(0x70)
    h[0:3] = b"GBS"
    h[3] = 1                                   # версия
    h[4] = 1                                   # одна песня
    h[5] = 1                                   # начинать с первой
    h[6:8] = struct.pack("<H", LOAD)
    h[8:10] = struct.pack("<H", LOAD)          # INIT
    h[10:12] = struct.pack("<H", LOAD + len(init))  # PLAY
    h[12:14] = struct.pack("<H", 0xCFFF)       # стек
    h[14] = 0                                  # TMA
    h[15] = 0                                  # TAC: vblank
    h[0x10:0x10 + 9] = b"CH%d test " % ch
    h[0x30:0x30 + 8] = b"m4pocket"
    return bytes(h) + bytes(body)


if __name__ == "__main__":
    x = int(sys.argv[1], 0) if len(sys.argv) > 1 else 0x6D6
    ch = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    path = sys.argv[3] if len(sys.argv) > 3 else "gbs_test.gbs"
    open(path, "wb").write(make(x, ch))
    print(f"{path}: канал {ch}, x={x:#05x}, ожидаемая частота "
          f"{131072 / (2048 - x):.1f} Гц")
