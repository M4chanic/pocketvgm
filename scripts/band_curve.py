#!/usr/bin/env python3
"""Разница АЧХ двух записей по октавным полосам.

Зачем отдельно от ab_compare. Тот считает ДОЛЮ энергии в полосе от полной,
и это правильно, когда сравниваешь два рендера одного тракта. Но против
записи с настоящего железа доля обманывает: у записи весь баланс смещён в
низ, доля всех прочих полос падает разом, и разница в 3 дБ показывается
как 30. Здесь вместо долей — абсолютный уровень в полосе, снятый полосовым
фильтром ffmpeg, а общий сдвиг усиления снимается нормировкой.

Нормировка обязательна: уровень записи с железа задан ручкой громкости и
входом звуковой карты, абсолютного смысла в нём нет. За ноль берётся
средняя разница в опорной полосе (по умолчанию 250-1000 Гц, где тракт
приставки заведомо ровный).

Использование:
    python3 scripts/band_curve.py эталон.wav наш.wav [-s пропуск] [-t секунд]

Читает всё, что умеет ffmpeg (wav, flac, mp3), — приводить заранее не надо.
"""

import argparse
import re
import subprocess
import sys

# Центры октавных полос. Выше 12 кГц смысла нет: у выхода 48 кГц там уже
# работает антиалиасинг, и мерялась бы не приставка, а он.
BANDS = [63, 125, 250, 500, 1000, 2000, 4000, 8000, 12000]


def band_level(path, freq, skip, dur):
    """Средний уровень в октавной полосе, дБ. None — если ffmpeg не смог."""
    if freq:
        # Полосовой дважды: у одиночного скаты пологие, и соседние
        # полосы протекают друг в друга.
        af = "bandpass=f={0}:width_type=o:w=1,bandpass=f={0}:width_type=o:w=1,".format(freq)
    else:
        af = ""
    cmd = ["ffmpeg", "-hide_banner", "-nostats"]
    if skip:
        cmd += ["-ss", str(skip)]
    if dur:
        cmd += ["-t", str(dur)]
    cmd += ["-i", path, "-af", af + "volumedetect", "-f", "null", "-"]
    p = subprocess.run(cmd, capture_output=True, text=True)
    m = re.search(r"mean_volume: ([-0-9.]+) dB", p.stderr)
    return float(m.group(1)) if m else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("reference", help="эталон (запись с железа или чужой рендер)")
    ap.add_argument("ours", help="наш рендер")
    ap.add_argument("-s", type=float, default=0.0, help="пропустить секунд")
    ap.add_argument("-t", type=float, default=30.0, help="секунд для анализа")
    ap.add_argument("--ref-band", default="250-1000",
                    help="опорная полоса для нормировки, Гц (по умолчанию 250-1000)")
    a = ap.parse_args()

    lo, hi = (int(x) for x in a.ref_band.split("-"))

    rows = []
    for f in BANDS:
        r = band_level(a.reference, f, a.s, a.t)
        o = band_level(a.ours, f, a.s, a.t)
        if r is None or o is None:
            print("ffmpeg не дал уровень на %d Гц — проверь пути" % f, file=sys.stderr)
            return 1
        rows.append((f, r, o, o - r))

    base = [d for f, _, _, d in rows if lo <= f <= hi]
    if not base:
        print("в опорную полосу %s не попало ни одного центра" % a.ref_band, file=sys.stderr)
        return 1
    shift = sum(base) / len(base)

    print("окно: %.0f-%.0f c, нормировка по %d-%d Гц (сдвиг %+.1f дБ снят)"
          % (a.s, a.s + a.t, lo, hi, shift))
    print()
    print("  полоса    эталон     наш   разница")
    worst = (0.0, 0)
    for f, r, o, d in rows:
        rel = d - shift
        mark = "  <<<" if abs(rel) >= 2.0 else ""
        print("  %5d Гц  %6.1f  %6.1f   %+6.1f дБ%s" % (f, r, o, rel, mark))
        if abs(rel) > abs(worst[0]):
            worst = (rel, f)
    print()
    if abs(worst[0]) < 1.0:
        print("вывод: АЧХ совпадает, наибольшее отклонение %+.1f дБ на %d Гц"
              % (worst[0], worst[1]))
    else:
        print("вывод: наибольшее отклонение %+.1f дБ на %d Гц — %s"
              % (worst[0], worst[1],
                 "у нас лишний верх" if worst[0] > 0 and worst[1] >= 2000 else
                 "у нас не хватает верха" if worst[0] < 0 and worst[1] >= 2000 else
                 "расхождение в низу"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
