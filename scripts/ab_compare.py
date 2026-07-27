#!/usr/bin/env python3
"""Сравнение нашего звука с эталонным рендерингом.

Побитово сравнивать бессмысленно: реализации чипов разные, фаза и
округление не совпадут никогда. Сравниваются признаки, по которым слышна
разница на самом деле:

  уровень   — общий RMS, ловит перекос громкости;
  баланс    — энергия по полосам, ловит лишний шум, глухость, пропавшие
              голоса (у пропавшего канала проседает своя полоса);
  огибающая — совпадает ли развитие во времени, ловит неверный темп и
              застрявшие ноты.

Полосы считаются алгоритмом Гёрцеля, без БПФ и без numpy — их в этом
окружении нет, а точности хватает с запасом.

Эталонные рендереры (собираются локально, в репозиторий не входят):
  VGM/VGZ — libvgm (ValleyBell), цель vgm2wav; cmake ставится через
            pip3 install --break-system-packages cmake, в системе его нет.
            Запуск: vgm2wav --samplerate 44100 файл.vgz эталон.wav
  GBS     — gbsplay (mmitch), configure с отключёнными звуковыми
            выходами; цель test падает, но бинарник собирается.
            Запуск: gbsplay -o wav -O эталон.wav -r 44100 -t СЕК -f 0 -g 0
                    файл.gbs ПОДПЕСНЯ ПОДПЕСНЯ
  NSF     — gme2wav, своя обёртка вокруг libgme; сборка описана в шапке
            scripts/gme2wav.c. Запуск: gme2wav файл.nsf ПОДПЕСНЯ СЕК выход.wav
  SID     — рендерера пока нет, такие файлы проверяются только на «не молчит»

Наш звук берётся из sim/chipbox_tb. ВНИМАНИЕ: у chipbox_tb ключи -t и -o
должны идти ДО --gbsfile, иначе разбор аргументов до них не доходит и
запись уходит в out.wav длиной по умолчанию.

Использование:
    python3 scripts/ab_compare.py эталон.wav наш.wav [-t секунды]
"""

import argparse
import math
import struct
import sys
import wave

# Логарифмическая сетка: от низа, где живут басы FM, до верха, где сидит
# шум и «песок» от неверной квантизации
BANDS = [
    (40, 80), (80, 160), (160, 320), (320, 640), (640, 1250),
    (1250, 2500), (2500, 5000), (5000, 10000), (10000, 20000),
]


def read_wav(path, seconds):
    with wave.open(path) as w:
        rate = w.getframerate()
        ch = w.getnchannels()
        width = w.getsampwidth()
        n = min(w.getnframes(), int(rate * seconds)) if seconds else w.getnframes()
        raw = w.readframes(n)
    if width != 2:
        sys.exit(f"{path}: поддержаны только 16-битные WAV (здесь {width * 8})")
    data = struct.unpack("<%dh" % (len(raw) // 2), raw)
    mono = [(data[i * ch] + data[i * ch + 1]) / 2 if ch > 1 else data[i]
            for i in range(len(data) // ch)]
    # Постоянную составляющую здесь НЕ трогаем: её снимают rms,
    # band_energy и envelope — каждая на том куске, который реально
    # считает. Раньше среднее вычиталось тут, по всему файлу, и стоило
    # вырезать из него отрезок, как чужая постоянная превращалась в
    # мнимую энергию: на тихой ступени NSF она дала 70% «энергии» в
    # 40-80 Гц и втрое завышенный уровень, из-за чего исправная шкала
    # громкости выглядела сломанной.
    return mono, rate


def _nodc(xs):
    """Кусок без постоянной составляющей: она не слышна, а в цифрах врёт."""
    if not xs:
        return xs
    m = sum(xs) / len(xs)
    return [x - m for x in xs]


def rms(xs):
    xs = _nodc(xs)
    return math.sqrt(sum(x * x for x in xs) / len(xs)) if xs else 0.0


def goertzel_power(xs, rate, freq):
    """Мощность на одной частоте. Дешевле полного спектра в разы."""
    k = 2.0 * math.cos(2.0 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for x in xs:
        s0 = x + k * s1 - s2
        s2, s1 = s1, s0
    return s1 * s1 + s2 * s2 - k * s1 * s2


def band_energy(xs, rate):
    """Энергия по полосам: в каждой берём несколько частот и усредняем."""
    xs = _nodc(xs)
    out = []
    for lo, hi in BANDS:
        if lo >= rate / 2:
            out.append(0.0)
            continue
        probes = [lo * (hi / lo) ** (i / 4) for i in range(5)]
        probes = [f for f in probes if f < rate / 2 * 0.95]
        e = sum(goertzel_power(xs, rate, f) for f in probes) / max(1, len(probes))
        out.append(e)
    return out


def envelope(xs, rate, step_ms=50):
    step = max(1, int(rate * step_ms / 1000))
    return [rms(xs[i:i + step]) for i in range(0, len(xs) - step, step)]


def correlation(a, b):
    n = min(len(a), len(b))
    if n < 4:
        return 0.0
    a, b = a[:n], b[:n]
    ma, mb = sum(a) / n, sum(b) / n
    va = sum((x - ma) ** 2 for x in a)
    vb = sum((x - mb) ** 2 for x in b)
    if va <= 0 or vb <= 0:
        return 0.0
    cov = sum((a[i] - ma) * (b[i] - mb) for i in range(n))
    return cov / math.sqrt(va * vb)


def db(x, ref):
    if ref <= 0 or x <= 0:
        return float("-inf") if x <= 0 else float("inf")
    return 20.0 * math.log10(x / ref)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("reference")
    ap.add_argument("ours")
    ap.add_argument("-t", type=float, default=5.0, help="секунд для анализа")
    a = ap.parse_args()

    ref, ref_rate = read_wav(a.reference, a.t)
    our, our_rate = read_wav(a.ours, a.t)
    if not ref or not our:
        sys.exit("пустой файл")

    r_rms, o_rms = rms(ref), rms(our)
    print(f"эталон: {len(ref)} отсчётов @ {ref_rate} Гц, RMS {r_rms:.0f}")
    print(f"наш:    {len(our)} отсчётов @ {our_rate} Гц, RMS {o_rms:.0f}")
    if o_rms == 0:
        print("\nНАШ ВЫХОД ПУСТОЙ — сравнивать нечего")
        return 1
    if r_rms == 0:
        print("\nЭТАЛОН ПУСТОЙ — проверь параметры рендеринга")
        return 1

    print(f"\nуровень: {db(o_rms, r_rms):+.1f} дБ относительно эталона")

    # Полосы нормируем на общий RMS: интересует баланс, а не громкость
    re = band_energy(ref, ref_rate)
    oe = band_energy(our, our_rate)
    rsum, osum = sum(re) or 1.0, sum(oe) or 1.0
    print("\nбаланс по полосам (доля энергии, разница в дБ):")
    worst = []
    for (lo, hi), r, o in zip(BANDS, re, oe):
        rp, op = r / rsum, o / osum
        d = db(op, rp)
        flag = "  <<<" if abs(d) >= 6 and max(rp, op) > 0.02 else ""
        print(f"  {lo:>5}-{hi:<6} Гц   эталон {rp * 100:5.1f}%   наш {op * 100:5.1f}%"
              f"   {d:+6.1f} дБ{flag}")
        if flag:
            worst.append((lo, hi, d))

    c = correlation(envelope(ref, ref_rate), envelope(our, our_rate))
    print(f"\nогибающая: корреляция {c:+.2f}")

    print("\nвывод:")
    if abs(db(o_rms, r_rms)) > 6:
        print("  - уровень расходится больше чем вдвое — проверь гейн в микшере")
    if c < 0.5:
        print("  - огибающая не совпадает: темп, зацикливание или застрявшие ноты")
    for lo, hi, d in worst:
        if d > 0:
            print(f"  - лишняя энергия в {lo}-{hi} Гц: шум, искажение или лишний голос")
        else:
            print(f"  - нехватка в {lo}-{hi} Гц: пропал голос или срезаны частоты")
    if not worst and c >= 0.5 and abs(db(o_rms, r_rms)) <= 6:
        print("  - заметных расхождений нет")
    return 0


if __name__ == "__main__":
    sys.exit(main())
