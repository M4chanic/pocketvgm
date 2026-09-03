#!/usr/bin/env python3
"""Диагностический NSF для VRC7: одна нота через $9010/$9030.

Настоящий NSF с VRC7 в корпусе один (Lagrange Point) и в репозиторий
не кладётся, а проводку шины 6502 -> транслятор OPLL проверить надо. Тот
же патч и та же нота, что у tone_check.py для VGM: пользовательский
патч-синус, block 4, fnum 0x16B (554.6 Гц); клавиша отпускается на 60-м
кадре и нажимается снова на 90-м. Рядом кладётся VGM с той же
последовательностью — для сравнения двух путей одним и тем же стендом.

    python3 scripts/demo_tunes/make_vrc7_test.py /tmp
    sim/chipbox_tb/chipbox_tb -t 2 -o nsf.wav --nsffile /tmp/vrc7_tone.nsf
    sim/chipbox_tb/chipbox_tb -t 2 -o vgm.wav /tmp/vrc7_tone.vgm
"""

import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from asm6502 import Asm

LOAD = 0x8000
CNT = 0x10
PATCH = (0x21, 0x21, 0x3F, 0x00, 0xF0, 0xF0, 0x07, 0x07)
REGS = list(enumerate(PATCH)) + [(0x30, 0x00), (0x10, 0x6B), (0x20, 0x19)]


def nsf(name, code, init, play):
    h = bytearray(0x80)
    h[0:5] = b"NESM\x1a"
    h[5], h[6], h[7] = 1, 1, 1
    h[8:10] = struct.pack("<H", LOAD)
    h[10:12] = struct.pack("<H", init)
    h[12:14] = struct.pack("<H", play)
    h[0x0E:0x0E + len(name)] = name.encode()
    h[0x6E:0x70] = struct.pack("<H", 16666)   # NTSC, 60 Гц
    h[0x7B] = 0x02                            # VRC7
    return bytes(h) + code


def wr(a, reg, val):
    """Запись регистра VRC7 с выдержкой: чипу нужно ~6 тактов после
    адреса и ~42 после данных, драйверы ждут циклом."""
    a.op("LDA", "imm", reg); a.op("STA", "abs", 0x9010)
    a.op("LDX", "imm", 2)
    a.label(f"w{a.n}a"); a.op("DEX"); a.op("BNE", "rel", f"w{a.n}a")
    a.op("LDA", "imm", val); a.op("STA", "abs", 0x9030)
    a.op("LDX", "imm", 9)
    a.label(f"w{a.n}b"); a.op("DEX"); a.op("BNE", "rel", f"w{a.n}b")
    a.n += 1


def build():
    a = Asm(LOAD)
    a.n = 0
    a.label("init")
    for r, v in REGS:
        wr(a, r, v)
    a.op("LDA", "imm", 0); a.op("STA", "zp", CNT)
    a.op("RTS")
    a.label("play")
    a.op("INC", "zp", CNT)
    a.op("LDA", "zp", CNT)
    a.op("CMP", "imm", 60); a.op("BNE", "rel", "not_off")
    wr(a, 0x20, 0x09)                          # отпустить
    a.label("not_off")
    a.op("LDA", "zp", CNT)
    a.op("CMP", "imm", 90); a.op("BNE", "rel", "done")
    wr(a, 0x20, 0x19)                          # нажать снова
    a.label("done")
    a.op("RTS")
    return a


def vgm(path):
    h = bytearray(0x100)
    h[0:4] = b"Vgm "
    h[8:12] = struct.pack("<I", 0x171)
    h[0x18:0x1C] = struct.pack("<I", 2 * 44100)
    h[0x34:0x38] = struct.pack("<I", 0x100 - 0x34)
    h[0x10:0x14] = struct.pack("<I", 3579545 | 0x80000000)
    h[0x84:0x88] = struct.pack("<I", 1789773)
    w = bytearray()
    for r, v in REGS:
        w += bytes([0x51, r, v])
    frame = 735
    w += b"\x61" + struct.pack("<H", 60 * frame)
    w += bytes([0x51, 0x20, 0x09])
    w += b"\x61" + struct.pack("<H", 30 * frame)
    w += bytes([0x51, 0x20, 0x19])
    w += b"\x61" + struct.pack("<H", 30 * frame)
    w.append(0x66)
    path.write_bytes(bytes(h) + bytes(w))


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    a = build()
    code = a.assemble()
    (out / "vrc7_tone.nsf").write_bytes(nsf("vrc7 tone", code, a.labels["init"], a.labels["play"]))
    vgm(out / "vrc7_tone.vgm")
    print("готово:", out / "vrc7_tone.nsf", out / "vrc7_tone.vgm", len(code), "байт кода")


if __name__ == "__main__":
    main()
