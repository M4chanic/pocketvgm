#!/usr/bin/env python3
"""Калибровка отношения громкостей чипов внутри одной системы.

Зачем не по абсолютному уровню. У libvgm абсолютной шкалы нет:
NormalizeOverallVolume домножает громкости ВСЕХ чипов на степень двойки,
пока оценка общей громкости не попадёт в (0x180, 0x300]. Для файла с
одним SN76489 множитель выходит 4, то есть +12 дБ; для файла с YM2612 —
2. Подгонять наш гейн под такой уровень значит вписывать в него чужой
множитель, который в файле с другим набором чипов будет другим.

Но множитель ОДИН на файл. Значит отношение чипов внутри одного файла от
него свободно, и калибровать надо именно отношения.

Как устроен замер. Синтетический VGM: чип A держит ровный тон, потом
тишина, потом чип B держит ровный тон — всё в одном файле, без таблицы
громкостей в заголовке. Считаем RMS каждого отрезка у эталона и у нас;
отношение отношений и есть поправка к гейну.

Тон берётся на двух уровнях громкости. Если отношение от уровня зависит,
дело не в гейне, а в форме таблицы громкости чипа — и это уже другая
находка, гейном её не лечат.

    make -C sim/chipbox_tb CLK=57120000
    python3 scripts/gain_ratio.py opn      YM2203: FM против SSG
    python3 scripts/gain_ratio.py md       Mega Drive: YM2612 против SN76489
"""

import math
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ab_compare as ab

ROOT = Path(__file__).resolve().parent.parent
TB = ROOT / "sim" / "chipbox_tb" / "chipbox_tb"
VGM2WAV = Path("/tmp/claude-0/-root-projects-m4pocket/97251955-7e25-4876-af9f-36e8651343cd/scratchpad/libvgm/build/bin/vgm2wav")
TMP = Path(tempfile.gettempdir())

SEG = 1.0  # секунд на отрезок


def hdr(clocks, secs):
    """Заголовок VGM 1.71 с заданными полями тактовых."""
    h = bytearray(0x100)
    h[0:4] = b"Vgm "
    h[8:12] = struct.pack("<I", 0x171)
    h[0x18:0x1C] = struct.pack("<I", int(secs * 44100))
    h[0x34:0x38] = struct.pack("<I", 0x100 - 0x34)
    for off, clk in clocks:
        h[off:off + 4] = struct.pack("<I", clk)
    return h


def wait(w, secs):
    n = int(secs * 44100)
    while n > 0:
        k = min(n, 65535)
        w += b"\x61" + struct.pack("<H", k)
        n -= k


def fm_regs(cmd, tl, block=5, fnum=1822):
    """Один операторный синус на первом FM-канале: алгоритм 7, слышен op1.
    tl — ослабление оператора, 0 громче всего."""
    w = bytearray()
    regs = [(0x30 + 4 * i, 0x01) for i in range(4)]
    regs += [(0x40, tl)] + [(0x40 + 4 * i, 0x7F) for i in (1, 2, 3)]
    regs += [(0x50 + 4 * i, 0x1F) for i in range(4)]
    regs += [(0x60 + 4 * i, 0x00) for i in range(4)]
    regs += [(0x70 + 4 * i, 0x00) for i in range(4)]
    regs += [(0x80 + 4 * i, 0x0F) for i in range(4)]
    regs += [(0xB0, 0x07), (0xA4, (block << 3) | (fnum >> 8)), (0xA0, fnum & 0xFF),
             (0x28, 0xF0)]
    for a, d in regs:
        w += bytes([cmd, a, d])
    return w


def fm_off(cmd):
    return bytes([cmd, 0x28, 0x00])


def ssg_regs(cmd, period, vol):
    w = bytearray()
    for a, d in ((0, period & 0xFF), (1, period >> 8), (7, 0x3E), (8, vol)):
        w += bytes([cmd, a, d])
    return w


def ssg_off(cmd):
    return bytes([cmd, 8, 0x00])


def psg_regs(vol):
    """SN76489: канал 0, период 0x0FE, громкость (0 — максимум)."""
    w = bytearray()
    for v in (0x80 | 0x0E, 0x0F, 0x90 | (vol & 0xF)):
        w += bytes([0x50, v])
    return w


def psg_off():
    return bytes([0x50, 0x9F])


def build_opn(path, loud):
    """YM2203: сначала FM, потом SSG. Один чип, одно поле тактовой."""
    cmd, clk = 0x55, 3_993_600
    w = bytearray()
    w += fm_regs(cmd, 0x00 if loud else 0x18)
    wait(w, SEG)
    w += fm_off(cmd)
    w += ssg_regs(cmd, 0x0FE, 0x0F if loud else 0x0B)
    wait(w, SEG)
    w += ssg_off(cmd)
    wait(w, 0.1)
    w.append(0x66)
    path.write_bytes(bytes(hdr([(0x44, clk)], 2 * SEG + 0.1)) + bytes(w))
    return ("FM", "SSG")


def build_md(path, loud):
    """Mega Drive: сначала FM YM2612, потом PSG SN76489."""
    w = bytearray()
    w += fm_regs(0x52, 0x00 if loud else 0x18)
    wait(w, SEG)
    w += fm_off(0x52)
    w += psg_regs(0 if loud else 4)
    wait(w, SEG)
    w += psg_off()
    wait(w, 0.1)
    w.append(0x66)
    path.write_bytes(bytes(hdr([(0x2C, 7_670_453), (0x0C, 3_579_545)], 2 * SEG + 0.1))
                     + bytes(w))
    return ("FM", "PSG")


def apu_regs(vol):
    """NES APU: первый импульсный канал, период 0x0FD, громкость 0..15."""
    w = bytearray()
    for a, d in ((0x15, 0x0F), (0x00, 0xB0 | (vol & 0xF)), (0x01, 0x00),
                 (0x02, 0xFD), (0x03, 0x00)):
        w += bytes([0xB4, a, d])
    return w


def apu_off():
    return bytes([0xB4, 0x15, 0x00])


def fds_regs(vol, freq=0x0200):
    """Дисковая приставка: синус в волновой таблице, огибающая выключена.

    Адреса — как их адресует VGM: 0x3F это $4023, 0x20-0x3E ложатся на
    $4080-$409E, 0x40-0x7F проходят в волновую таблицу как есть.
    """
    import math as _m
    w = bytearray()
    def r(a, d):
        w.extend([0xB4, a, d])
    r(0x3F, 0x02)                 # разрешение ввода-вывода
    r(0x29, 0x80)                 # разрешить запись таблицы
    for i in range(64):
        r(0x40 + i, int(31.5 + 31.5 * _m.sin(2 * _m.pi * i / 64)) & 0x3F)
    r(0x29, 0x00)                 # закрыть запись, общая громкость полная
    r(0x2A, 0xFF)                 # общая скорость огибающей
    r(0x20, 0x80 | (vol & 0x3F))  # огибающая выключена, громкость задана
    r(0x24, 0x80)                 # модулятор: сила 0
    r(0x27, 0x80)                 # модулятор остановлен
    r(0x22, freq & 0xFF)
    r(0x23, (freq >> 8) & 0x0F)   # запуск
    return w


def fds_off():
    return bytes([0xB4, 0x23, 0x80])   # остановить волновую таблицу


def build_fds(path, loud):
    """Famicom: сначала импульсный канал APU, потом волновая таблица FDS."""
    w = bytearray()
    w += apu_regs(15 if loud else 7)
    wait(w, SEG)
    w += apu_off()
    w += fds_regs(32 if loud else 16)
    wait(w, SEG)
    w += fds_off()
    wait(w, 0.1)
    w.append(0x66)
    # старший бит поля NES APU — признак дисковой приставки
    path.write_bytes(bytes(hdr([(0x84, 1_789_773 | 0x80000000)], 2 * SEG + 0.1))
                     + bytes(w))
    return ("APU", "FDS")


BUILDERS = {"opn": build_opn, "md": build_md, "fds": build_fds}


def rms(xs):
    if not xs:
        return 0.0
    m = sum(xs) / len(xs)
    return math.sqrt(sum((v - m) ** 2 for v in xs) / len(xs))


def seg_rms(path):
    """RMS первого и второго отрезка, с отступом от краёв: атака и
    затухание в замер попадать не должны."""
    x, rate = ab.read_wav(str(path), None)
    out = []
    for k in (0, 1):
        a = int(rate * (k * SEG + 0.25))
        b = int(rate * (k * SEG + SEG - 0.05))
        out.append(rms(x[a:b]))
    return out


def db(a, b):
    return 20 * math.log10(a / b) if a > 0 and b > 0 else float("nan")


def main():
    which = sys.argv[1] if len(sys.argv) > 1 else "opn"
    if which not in BUILDERS:
        sys.exit(f"известны: {', '.join(BUILDERS)}")
    if not TB.exists():
        sys.exit(f"нет стенда {TB}; make -C sim/chipbox_tb CLK=57120000")
    ref_bin = next((p for p in (VGM2WAV, TMP / "libvgm" / "build" / "bin" / "vgm2wav")
                    if p.exists()), None)
    if ref_bin is None:
        sys.exit("не найден vgm2wav; путь задаётся в шапке скрипта")

    print(f"{'уровень':8} {'чип A':>10} {'чип B':>10}   отношение A/B")
    print(f"{'':8} {'':>10} {'':>10}   эталон   наше   поправка")
    for loud in (True, False):
        tag = "громко" if loud else "тише"
        src = TMP / f"gr_{which}_{int(loud)}.vgm"
        names = BUILDERS[which](src, loud)
        ref = TMP / f"gr_{which}_{int(loud)}_ref.wav"
        our = TMP / f"gr_{which}_{int(loud)}_our.wav"
        subprocess.run([str(ref_bin), "--samplerate", "48000", str(src), str(ref)],
                       check=True, capture_output=True)
        subprocess.run([str(TB), str(src), "-o", str(our), "-t", str(2 * SEG + 0.05)],
                       check=True, capture_output=True)
        ra, rb = seg_rms(ref)
        oa, ob = seg_rms(our)
        r_ref, r_our = db(ra, rb), db(oa, ob)
        print(f"{tag:8} {names[0]:>10} {names[1]:>10}   "
              f"{r_ref:+6.1f} {r_our:+6.1f}   {r_ref - r_our:+6.1f} дБ")
        print(f"{'':8} RMS эталон {ra:8.0f} {rb:8.0f}   наше {oa:8.0f} {ob:8.0f}")


if __name__ == "__main__":
    main()
