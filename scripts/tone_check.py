#!/usr/bin/env python3
"""Калибровка частоты чипа по одной ноте: во сколько раз мы промахнулись.

Полосный отчёт `ab_suite.py` отвечает на вопрос «где не так», но не «во
сколько раз». На живой музыке за три секунды звучит десяток нот, полосы
размазаны, и октава вниз читается как нехватка верха — на OPN это увело
в сторону три захода подряд. Здесь играется ровно одна нота с известными
параметрами, и пик ищется Гёрцелем по сетке 1/48 октавы, так что ответ
получается точным числом:

    YM2203 block=4 fnum=1000: эталон 421.5 Гц, наше 210.8 Гц,
    отношение 0.500 (-12.0 полутона)

Порядок работы: сначала снять модель делителей с эталонной реализации,
потом проверить её здесь, и только затем слушать корпус.

    python3 scripts/tone_check.py            все проверки
    python3 scripts/tone_check.py ym2203     только один чип
"""

import math
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ab_compare as ab
import ab_suite

ROOT = Path(__file__).resolve().parent.parent
TB = ROOT / "sim" / "chipbox_tb" / "chipbox_tb"
TMP = Path(tempfile.gettempdir())

# Чип -> (команда VGM, смещение клока в заголовке, тактовая)
CHIPS = {
    "ym2203": (0x55, 0x44, 3_993_600),
    "ym2608": (0x56, 0x48, 7_987_200),
    "ay8910": (0xA0, 0x74, 1_789_772),
    "scc": (0xD2, 0x9C, 1_789_772),
}


def header(clk_off, clk, secs):
    h = bytearray(0x100)
    h[0:4] = b"Vgm "
    h[8:12] = struct.pack("<I", 0x171)
    h[0x18:0x1C] = struct.pack("<I", int(secs * 44100))
    h[0x34:0x38] = struct.pack("<I", 0x100 - 0x34)
    h[clk_off:clk_off + 4] = struct.pack("<I", clk)
    return h


def tail(secs):
    """Пауза на всю длину и конец потока."""
    w = bytearray()
    n = int(secs * 44100)
    while n > 0:
        k = min(n, 65535)
        w += b"\x61" + struct.pack("<H", k)
        n -= k
    w.append(0x66)
    return w


def fm_note(path, chip, block, fnum, secs=2.0):
    """Один операторный синус на первом FM-канале: алгоритм 7, слышен op1."""
    cmd, off, clk = CHIPS[chip]
    w = bytearray()
    regs = [(0x30 + 4 * i, 0x01) for i in range(4)]          # MUL=1
    regs += [(0x40, 0x00)] + [(0x40 + 4 * i, 0x7F) for i in (1, 2, 3)]
    regs += [(0x50 + 4 * i, 0x1F) for i in range(4)]          # атака мгновенная
    regs += [(0x60 + 4 * i, 0x00) for i in range(4)]
    regs += [(0x70 + 4 * i, 0x00) for i in range(4)]
    regs += [(0x80 + 4 * i, 0x0F) for i in range(4)]
    regs += [(0xB0, 0x07),                                    # алгоритм 7
             (0xA4, (block << 3) | (fnum >> 8)), (0xA0, fnum & 0xFF),
             (0x28, 0xF0)]                                    # key on
    for a, d in regs:
        w += bytes([cmd, a, d])
    open(path, "wb").write(bytes(header(off, clk, secs)) + bytes(w + tail(secs)))


def ssg_note(path, chip, period, secs=2.0):
    """Один тон на первом канале SSG/AY."""
    cmd, off, clk = CHIPS[chip]
    w = bytearray()
    for a, d in ((0, period & 0xFF), (1, period >> 8), (7, 0x3E), (8, 0x0F)):
        w += bytes([cmd, a, d])
    open(path, "wb").write(bytes(header(off, clk, secs)) + bytes(w + tail(secs)))


def ay_env_note(path, chip, period, shape=0x08, secs=2.0):
    """Огибающая AY как волна: тон и шум выключены, громкость из огибающей.

    Классический приём MSX — короткий период огибающей сам становится
    слышимым тоном. Проверяет делитель огибающей, до которого обычный
    тоновый тест не достаёт.
    """
    cmd, off, clk = CHIPS[chip]
    w = bytearray()
    for a, d in ((7, 0x3F), (8, 0x10), (11, period & 0xFF), (12, period >> 8),
                 (13, shape)):
        w += bytes([cmd, a, d])
    open(path, "wb").write(bytes(header(off, clk, secs)) + bytes(w + tail(secs)))


def scc_note(path, period, secs=2.0):
    """Синус в волновой таблице первого канала SCC.

    Порты команды 0xD2: 0 — волновая таблица, 1 — частота, 2 —
    громкость, 3 — включение каналов.
    """
    cmd, off, clk = CHIPS["scc"]
    w = bytearray()

    def scc(port, reg, val):
        w.extend([cmd, port, reg, val & 0xFF])

    for i in range(32):
        scc(0, i, int(round(127 * math.sin(2 * math.pi * i / 32))))
    scc(1, 0, period & 0xFF)
    scc(1, 1, (period >> 8) & 0x0F)
    scc(2, 0, 0x0F)
    scc(3, 0, 0x01)
    open(path, "wb").write(bytes(header(off, clk, secs)) + bytes(w + tail(secs)))


def peak(path, secs):
    """Частота самого сильного тона, сетка 1/48 октавы."""
    s, rate = ab.read_wav(path, secs)
    best = (0.0, 0.0)
    f = 30.0
    while f < 12000:
        w = 2 * math.cos(2 * math.pi * f / rate)
        s1 = s2 = 0.0
        for x in s:
            s0 = x + w * s1 - s2
            s2, s1 = s1, s0
        m = s1 * s1 + s2 * s2 - w * s1 * s2
        if m > best[0]:
            best = (m, f)
        f *= 2 ** (1 / 48)
    return best[1]


def compare(vgm2wav, label, secs=2.0):
    ref, our = TMP / "m4tone_ref.wav", TMP / "m4tone_our.wav"
    src = TMP / "m4tone.vgm"
    for f in (ref, our):
        f.unlink(missing_ok=True)
    subprocess.run([str(vgm2wav), "--samplerate", "44100", str(src), str(ref)],
                   capture_output=True, timeout=300)
    subprocess.run([str(TB), "-t", str(secs), "-o", str(our), str(src)],
                   capture_output=True, timeout=600)
    if not ref.exists() or not our.exists():
        print(f"{label:34} не отрендерилось")
        return
    a, b = peak(str(ref), secs * 0.75), peak(str(our), secs * 0.75)
    if not a or not b:
        print(f"{label:34} тишина")
        return
    print(f"{label:34} эталон {a:8.1f} Гц, наше {b:8.1f} Гц, "
          f"отношение {b / a:5.3f} ({12 * math.log2(b / a):+5.1f} полутона)")


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else None
    if not TB.exists():
        sys.exit(f"нет стенда: {TB} — соберите sim/chipbox_tb")
    ab_suite.find_tools()
    if not ab_suite.VGM2WAV:
        sys.exit("нет vgm2wav — соберите libvgm, см. шапку ab_compare.py")
    v = ab_suite.VGM2WAV
    src = TMP / "m4tone.vgm"
    for chip in ("ym2203", "ym2608"):
        if only and chip != only:
            continue
        for block, fnum in ((4, 1000), (5, 1200)):
            fm_note(src, chip, block, fnum)
            compare(v, f"{chip} FM block={block} fnum={fnum}")
    for chip in ("ym2203", "ym2608", "ay8910"):
        if only and chip != only:
            continue
        for period in (200, 400):
            ssg_note(src, chip, period)
            compare(v, f"{chip} SSG период={period}")
    for chip in ("ay8910", "ym2203", "ym2608"):
        if only and chip != only:
            continue
        for period in (16, 32):
            ay_env_note(src, chip, period)
            compare(v, f"{chip} огибающая период={period}")
    if not only or only == "scc":
        for period in (256, 512):
            scc_note(src, period)
            compare(v, f"scc период={period}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
