#!/usr/bin/env python3
"""Призраки от пересчёта частоты дискретизации: сколько их и где.

Зачем отдельный инструмент. Полосный отчёт `ab_compare.py` этот дефект не
видит вообще: пересчёт выборкой-с-удержанием меняет уровень на 0.1-0.6 дБ,
а огибающую не меняет совсем. Видно его только по одной ноте — по
НЕАРМОНИЧЕСКИМ пикам, которых у чистого тона быть не должно.

Где они садятся. Если чип отдаёт сэмплы на частоте fc, а читают их на fo,
и между ними стоит выборка с удержанием, то тон f даёт пики на
|fc - fo| +- f. Для YM2612 при 7.67 МГц это fc = 7670453/144 = 53267 Гц,
у нас fo = 48000, разностная частота 5267 Гц.

Нота берётся так, чтобы её гармоники не попали на призраки: при f = 2 кГц
гармоники стоят на 4, 6, 8, 10 кГц, а призраки на 3.3 и 7.3 кГц.

Мерить ОБЯЗАТЕЛЬНО на тактовой железа — иначе и частота чипа, и частота
строба другие, и считать будет нечего:

    make -C sim/chipbox_tb CLK=57120000
    python3 scripts/image_check.py sim/chipbox_tb/chipbox_tb
"""

import math
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ab_compare as ab
import tone_check as tc

TMP = Path(tempfile.gettempdir())

# YM2612: команда 0x52, поле тактовой 0x2C. Частота сэмплов = clk/144.
YM2612 = (0x52, 0x2C, 7_670_453)
FM_DIV = 144
OUT_RATE = 48000.0


def peak_near(x, rate, f, span=1.03, steps=25):
    """Положение и амплитуда пика около f: грубая сетка, потом точная.

    Одной грубой сетки не хватает. С шагом 1.6% найденная частота уезжала
    на десяток герц, и посчитанные от неё гармоники промахивались мимо
    своих пиков — а призрак, наоборот, попадал в гармонику. Из-за этого
    первый замер показал «призрак -3.1 дБ», хотя мерил третью гармонику.
    """
    best = (0.0, f)
    for i in range(steps):
        g = f * span ** (2.0 * i / (steps - 1) - 1.0)
        a = amp(x, rate, g)
        if a > best[0]:
            best = (a, g)
    # уточнение: +-3% линейно, шаг 0.02%
    f0 = best[1]
    for i in range(-150, 151):
        g = f0 * (1.0 + i * 0.0002)
        a = amp(x, rate, g)
        if a > best[0]:
            best = (a, g)
    return best


def amp(x, rate, f):
    """Амплитуда на частоте f: Гёрцель с окном Ханна."""
    n = len(x)
    w = 2.0 * math.pi * f / rate
    coeff = 2.0 * math.cos(w)
    s1 = s2 = 0.0
    for i, v in enumerate(x):
        h = 0.5 - 0.5 * math.cos(2.0 * math.pi * i / (n - 1))
        s0 = coeff * s1 - s2 + v * h
        s2, s1 = s1, s0
    p = s1 * s1 + s2 * s2 - coeff * s1 * s2
    return math.sqrt(max(p, 0.0)) * 2.0 / n


def db(a, ref):
    return 20.0 * math.log10(a / ref) if a > 0 and ref > 0 else float("-inf")


def main():
    tb = Path(sys.argv[1] if len(sys.argv) > 1 else "sim/chipbox_tb/chipbox_tb")
    if not tb.exists():
        sys.exit(f"не найден стенд {tb}")

    cmd, off, clk = YM2612
    fc = clk / FM_DIV
    delta = abs(fc - OUT_RATE)
    print(f"YM2612: тактовая {clk} Гц, сэмплы {fc:.1f} Гц")
    print(f"выход: {OUT_RATE:.0f} Гц, разностная частота {delta:.0f} Гц\n")

    src = TMP / "img_tone.vgm"
    out = TMP / "img_tone.wav"
    # block/fnum подобраны около 2 кГц; точная частота меряется по факту.
    # Нота обязана стоять в стороне от delta/2, delta/3, delta/4...: при
    # f = delta/4 = 1317 Гц нижний призрак ложится ровно на третью
    # гармонику, и замер теряет смысл (на этом первый прогон и сорвался).
    tc.CHIPS["ym2612"] = YM2612
    tc.fm_note(src, "ym2612", 5, 1822, secs=1.0)
    subprocess.run([str(tb), str(src), "-o", str(out), "-t", "1.0"],
                   check=True, capture_output=True)

    x, rate = ab.read_wav(str(out), None)
    # вторая половина: атака и переходный процесс не мешают
    x = x[len(x) // 2:]
    print(f"записано {len(x)} отсчётов @ {rate} Гц")

    # Ищем ноту в узком окне около ожидаемой: широкий поиск может принять
    # за ноту её же призрак, если тот окажется сильнее в своей полосе.
    a0, f0 = peak_near(x, rate, 2000.0, span=1.25, steps=41)
    print(f"нота: {f0:.1f} Гц\n")

    ghosts = {"призрак снизу": abs(delta - f0), "призрак сверху": delta + f0}
    harm = {f"гармоника {k}": f0 * k for k in (2, 3, 4, 5)}
    # Столкновение призрака с гармоникой делает замер бессмысленным
    for gn, gf in ghosts.items():
        for hn, hf in harm.items():
            if hf > 0 and abs(gf - hf) / hf < 0.02:
                print(f"ВНИМАНИЕ: {gn} ({gf:.0f} Гц) совпал с {hn} "
                      f"({hf:.0f} Гц) — возьмите другую ноту\n")

    print("       что            частота    уровень")
    worst = 0.0
    for name, f in [("нота", f0), *harm.items(), *ghosts.items()]:
        if f < 40 or f > rate / 2 - 200:
            continue
        a = amp(x, rate, f)
        mark = ""
        if name.startswith("призрак"):
            mark = "  <-- неармонический"
            worst = max(worst, a)
        print(f"  {name:<14} {f:8.0f}  {db(a, a0):+7.1f} дБ{mark}")
    print(f"\nхудший призрак: {db(worst, a0):+.1f} дБ относительно ноты")


if __name__ == "__main__":
    main()
