#!/usr/bin/env python3
"""Кривая тракта из CSV, который выдаёт mdfourier с ключом -C.

MDFourier сравнивает две записи одного и того же тонового сигнала и пишет
таблицу «тип, частота, разница». Точек там десятки тысяч — по одной на
каждую найденную гармонику каждого тона, — и глазами это не читается.
Здесь они сводятся в третьоктавные полосы по медиане.

ЗНАК. В mdfourier (diff.c) разница считается как |ref| - |comp| в дБFS,
где обе величины отрицательные, а модуль — это «насколько ниже полной
шкалы». Значит его плюс уже означает «эталон тише», то есть «сравниваемый
громче». Здесь то же самое и выводится как есть: ПЛЮС — наш ГРОМЧЕ
эталона, минус — тише. Так же, как в band_curve.py.

Перепутать тут легко, поэтому проверка на известном случае: если эталон —
запись с железа, а сравниваемый — libvgm без выходного фильтра, то на
4 кГц и выше должен получиться ПЛЮС: приставка там режет, а libvgm нет.

Типы блоков у профиля Mega Drive: FM (96 тонов), SPSG (40 тонов),
SPSG_Ramp (400 ступеней), Noise (16 вариантов). Смотреть их надо порознь:
FM и PSG идут в приставке разными путями и смешиваются в разной
пропорции, поэтому расхождение между их кривыми — это не ошибка замера,
а разница в микшировании.

Использование:
    mdfourier -P profiles/mdfblocksGEN.mfn -r железо.flac -c наш.wav -C
    python3 scripts/mdf_curve.py путь/к/результату.csv [-T FM,SPSG]
"""

import argparse
import collections
import csv
import sys

MIN_POINTS = 5      # полосы, где точек меньше, не показываем — шум


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("csv_path")
    ap.add_argument("-T", default="FM,SPSG",
                    help="типы блоков через запятую (FM, SPSG, SPSG_Ramp, Noise)")
    ap.add_argument("--lo", type=float, default=40.0, help="нижняя частота")
    ap.add_argument("--hi", type=float, default=20000.0, help="верхняя частота")
    a = ap.parse_args()

    rows = []
    try:
        with open(a.csv_path) as fh:
            r = csv.reader(fh)
            next(r, None)
            for rec in r:
                if len(rec) < 3:
                    continue
                rows.append((rec[0].strip(), float(rec[1]), float(rec[2])))
    except OSError as e:
        print("не открыть %s: %s" % (a.csv_path, e), file=sys.stderr)
        return 1

    if not rows:
        print("в CSV нет данных", file=sys.stderr)
        return 1

    # Границы третьоктавных полос
    edges = []
    f = a.lo
    while f < a.hi:
        edges.append(f)
        f *= 2 ** (1 / 3)

    def band_of(fr):
        for i in range(len(edges) - 1):
            if edges[i] <= fr < edges[i + 1]:
                return i
        return None

    print("точек всего: %d (%s)" % (
        len(rows), ", ".join("%s %d" % (k, v) for k, v in
                             collections.Counter(t for t, _, _ in rows).most_common())))

    for want in [t.strip() for t in a.T.split(",")]:
        acc = collections.defaultdict(list)
        for t, fr, d in rows:
            if t != want:
                continue
            b = band_of(fr)
            if b is not None:
                acc[b].append(d)
        if not acc:
            continue
        print("\n=== %s: наш относительно эталона, дБ ===" % want)
        for b in sorted(acc):
            v = sorted(acc[b])
            if len(v) < MIN_POINTS:
                continue
            # У mdfourier плюс уже означает «эталон тише», то есть
            # «наш громче» — разворачивать не надо
            rel = v[len(v) // 2]
            bar = "#" * min(20, int(abs(rel) * 2))
            print("  %7.0f Гц  n=%-5d  %+6.2f  %s" % (edges[b], len(v), rel, bar))
    return 0


if __name__ == "__main__":
    sys.exit(main())
