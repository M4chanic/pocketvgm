#!/usr/bin/env python3
"""Диагностические NSF для APU: огибающая, шкала громкости, тишина.

Тот же приём, что вытащил дефекты у HuC6280, OPN и SCC: не спорить со
спектром живой музыки, а сыграть одну вещь с известными параметрами и
сравнить с эталоном числом. Рипы для этого не годятся — там сразу
несколько голосов и своя аранжировка.

    python3 scripts/demo_tunes/make_apu_test.py /tmp        три файла

Что с ними делать (эталон — gme2wav, см. scripts/gme2wav.c):
    gme2wav /tmp/apu_volume.nsf 1 8 ref.wav
    sim/chipbox_tb/chipbox_tb -t 8 -o our.wav --nsffile /tmp/apu_volume.nsf
    python3 scripts/ab_compare.py ref.wav our.wav

  apu_envelope.nsf — одна нота, огибающая зациклена: период спада
      напрямую показывает частоту кадрового счётчика APU;
  apu_volume.nsf   — 15 ступеней постоянной громкости по полсекунды:
      один рендер даёт весь закон громкости;
  apu_triangle.nsf — треугольник включается и выключается каждые
      полсекунды: ловит канал, который не замолкает по счётчикам;
  apu_silence.nsf  — $4015=0 и больше ничего: проверка на то, что при
      выключенном звуке на выходе действительно ноль.
"""

import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from asm6502 import Asm

LOAD = 0x8000
CNT, VOL = 0x10, 0x11          # ячейки нулевой страницы под счётчики


def nsf(name, code, init, play):
    h = bytearray(0x80)
    h[0:5] = b"NESM\x1a"
    h[5], h[6], h[7] = 1, 1, 1
    h[8:10] = struct.pack("<H", LOAD)
    h[10:12] = struct.pack("<H", init)
    h[12:14] = struct.pack("<H", play)
    h[0x0E:0x0E + len(name)] = name.encode()
    h[0x6E:0x70] = struct.pack("<H", 16666)   # NTSC, 60 Гц
    return bytes(h) + code


def envelope_test():
    """Нота с зацикленной огибающей: спад повторяется сам собой."""
    a = Asm(LOAD)
    a.label("init")
    a.op("LDA", "imm", 0x01); a.op("STA", "abs", 0x4015)
    a.op("LDA", "imm", 0x08); a.op("STA", "abs", 0x4001)   # свип выключен
    # $4000 = DD L C VVVV: скважность 2, L=1 (длина стоит, огибающая
    # зациклена), C=0 (громкость из огибающей), делитель 10
    a.op("LDA", "imm", 0xAA); a.op("STA", "abs", 0x4000)
    a.op("LDA", "imm", 0xFE); a.op("STA", "abs", 0x4002)
    a.op("LDA", "imm", 0x00); a.op("STA", "abs", 0x4003)
    a.op("RTS")
    a.label("play")
    a.op("RTS")
    return a


def volume_test():
    """15 ступеней постоянной громкости, по 30 кадров на ступень."""
    a = Asm(LOAD)
    a.label("init")
    a.op("LDA", "imm", 0x01); a.op("STA", "abs", 0x4015)
    a.op("LDA", "imm", 0x08); a.op("STA", "abs", 0x4001)
    a.op("LDA", "imm", 0xFE); a.op("STA", "abs", 0x4002)
    a.op("LDA", "imm", 0x00); a.op("STA", "abs", 0x4003)
    a.op("LDA", "imm", 30); a.op("STA", "zp", CNT)
    a.op("LDA", "imm", 15); a.op("STA", "zp", VOL)
    a.op("LDA", "imm", 0x3F); a.op("STA", "abs", 0x4000)
    a.op("RTS")
    a.label("play")
    a.op("DEC", "zp", CNT)
    a.op("LDA", "zp", CNT)
    a.op("BNE", "rel", "done")
    a.op("LDA", "imm", 30); a.op("STA", "zp", CNT)
    a.op("DEC", "zp", VOL)
    a.op("LDA", "zp", VOL)
    a.op("BNE", "rel", "setvol")
    a.op("LDA", "imm", 15); a.op("STA", "zp", VOL)
    a.label("setvol")
    a.op("LDA", "zp", VOL)
    a.op("CLC"); a.op("ADC", "imm", 0x30)      # C=1 (постоянная), длина стоит
    a.op("STA", "abs", 0x4000)
    a.label("done")
    a.op("RTS")
    return a


def triangle_test():
    """Треугольник включается и выключается через $4015, по полсекунды.

    Канал известен тем, что при неверной реализации счётчиков (длины и
    линейного) продолжает гудеть после выключения. На музыке это видно
    как лишний голос в нижней середине и как перевёрнутые акценты.
    """
    a = Asm(LOAD)
    a.label("init")
    a.op("LDA", "imm", 0x04); a.op("STA", "abs", 0x4015)   # только треугольник
    a.op("LDA", "imm", 0xFF); a.op("STA", "abs", 0x4008)   # линейный счётчик, halt
    a.op("LDA", "imm", 0x54); a.op("STA", "abs", 0x400A)   # период
    a.op("LDA", "imm", 0x00); a.op("STA", "abs", 0x400B)   # старт
    a.op("LDA", "imm", 30); a.op("STA", "zp", CNT)
    a.op("LDA", "imm", 0x04); a.op("STA", "zp", VOL)       # текущее значение $4015
    a.op("RTS")
    a.label("play")
    a.op("DEC", "zp", CNT)
    a.op("LDA", "zp", CNT)
    a.op("BNE", "rel", "done")
    a.op("LDA", "imm", 30); a.op("STA", "zp", CNT)
    a.op("LDA", "zp", VOL)
    a.op("BEQ", "rel", "turnon")
    a.op("LDA", "imm", 0x00)                                # выключаем
    a.op("STA", "abs", 0x4015); a.op("STA", "zp", VOL)
    a.op("RTS")
    a.label("turnon")
    a.op("LDA", "imm", 0x04); a.op("STA", "abs", 0x4015); a.op("STA", "zp", VOL)
    a.op("LDA", "imm", 0xFF); a.op("STA", "abs", 0x4008)
    a.op("LDA", "imm", 0x00); a.op("STA", "abs", 0x400B)
    a.label("done")
    a.op("RTS")
    return a


def silence_test():
    """Ничего не включаем: на выходе обязан быть ноль."""
    a = Asm(LOAD)
    a.label("init")
    a.op("LDA", "imm", 0x00); a.op("STA", "abs", 0x4015)
    a.op("RTS")
    a.label("play")
    a.op("RTS")
    return a


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    out.mkdir(parents=True, exist_ok=True)
    for name, build in (("apu_envelope", envelope_test),
                        ("apu_volume", volume_test),
                        ("apu_triangle", triangle_test),
                        ("apu_silence", silence_test)):
        a = build()
        code = a.assemble()
        p = out / f"{name}.nsf"
        p.write_bytes(nsf(name, code, a.labels["init"], a.labels["play"]))
        print(f"{p}  {len(code)} байт кода")
    return 0


if __name__ == "__main__":
    sys.exit(main())
