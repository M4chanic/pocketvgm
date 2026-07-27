#!/usr/bin/env python3
"""Прогон тестового корпуса через наше ядро и эталон, сводка по чипам.

Отвечает на вопрос «где мы звучим не так» одной таблицей, без прослушивания
и без железа. За день отладки это трижды экономило круг: числа показывали
дефект там, где на слух он казался другим, и опровергали гипотезы за минуты.

Что нужно рядом (собирается локально, см. шапку ab_compare.py):
  vgm2wav из libvgm — эталон для VGM/VGZ
  gbsplay           — эталон для GBS
  gme2wav           — эталон для NSF (обёртка вокруг libgme)
  sim/chipbox_tb    — наше ядро

ВАЖНО про тактовую стенда: домен Game Boy требует 8.388608 МГц, а стенд по
умолчанию тактуется 8 МГц — инкремент фазы переполняется, и домен идёт в 21
раз медленнее. Для GBS собирать стенд как make CLK=57120000, иначе замеры
недействительны. Скрипт предупредит, если увидит GBS на быстром стенде.

    python3 scripts/ab_suite.py              все чипы, по 2 трека
    python3 scripts/ab_suite.py huc6280      один чип
    python3 scripts/ab_suite.py -n 4         по 4 трека на чип
"""

import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ab_compare as ab
import corpus

ROOT = Path(__file__).resolve().parent.parent
TB = ROOT / "sim" / "chipbox_tb" / "chipbox_tb"
SCRATCH = Path(tempfile.gettempdir()) / "m4_absuite"

# Потолок счёта на трек. Был полчаса — и один тяжёлый файл вешал всю
# таблицу, ради ничего. Лучше пометить трек и идти дальше.
TRACK_TIMEOUT = 240

# Чем рендерить эталон. Для SID подходящего рендерера пока нет — такие
# файлы прогоняются только на «не молчит», без сравнения.
VGM2WAV = None
GBSPLAY = None
GME2WAV = None


def _find(name):
    for base in (Path("/tmp"), Path.home(), ROOT.parent):
        for p in base.rglob(name):
            if p.is_file() and p.stat().st_mode & 0o111:
                return p
    return None


def find_tools():
    global VGM2WAV, GBSPLAY, GME2WAV
    VGM2WAV = VGM2WAV or _find("vgm2wav")
    GBSPLAY = GBSPLAY or _find("gbsplay")
    GME2WAV = GME2WAV or _find("gme2wav")


def render_ref(track, out, seconds):
    """Эталон. None — рендерера для этого формата нет."""
    ext = track.suffix.lower()
    if ext in (".vgm", ".vgz") and VGM2WAV:
        subprocess.run([str(VGM2WAV), "--samplerate", "44100", str(track), str(out)],
                       capture_output=True, timeout=300)
        return out.exists()
    if ext == ".gbs" and GBSPLAY:
        subprocess.run([str(GBSPLAY), "-o", "wav", "-O", str(out), "-r", "44100",
                        "-t", str(seconds), "-f", "0", "-g", "0", "-q",
                        str(track), "1", "1"], capture_output=True, timeout=300)
        return out.exists()
    if ext == ".nsf" and GME2WAV:
        # Подпесня 1: стенд в nsf_file всегда заводит первую (LDA #0 в
        # стабе), и номер здесь считается с единицы — как у gbsplay.
        subprocess.run([str(GME2WAV), str(track), "1", str(seconds), str(out)],
                       capture_output=True, timeout=300)
        return out.exists()
    return False


def render_ours(track, out, seconds):
    ext = track.suffix.lower()
    # ключи -t и -o обязаны идти ДО --gbsfile: дальше разбор не доходит
    cmd = [str(TB), "-t", str(seconds), "-o", str(out)]
    if ext == ".gbs":
        cmd += ["--gbsfile", str(track)]
    elif ext == ".nsf":
        cmd += ["--nsffile", str(track)]   # стенду нужен свой ключ, не VGM
    elif ext == ".sid":
        cmd += ["--sidfile", str(track)]
    else:
        cmd += [str(track)]
    try:
        subprocess.run(cmd, capture_output=True, timeout=TRACK_TIMEOUT)
    except subprocess.TimeoutExpired:
        return False
    return out.exists()


def verdict(level_db, worst_db, corr, worst_share):
    """worst_share — доля энергии в худшей полосе, у нас или у эталона.

    Без неё инструмент кричит на полосах, где энергии почти нет: 2.8%
    против 0.1% это +35 дБ, но на слух ничего не значит. Порог отсекает
    такие случаи, а огибающая ловит то, что действительно разъехалось.
    """
    if worst_db is None:
        return "нет эталона"
    big = worst_share >= 0.08          # полоса весит хотя бы 8% энергии
    if corr < 0.5 or abs(level_db) > 6 or (big and abs(worst_db) >= 10):
        return "РАСХОЖДЕНИЕ"
    if big and abs(worst_db) >= 6:
        return "заметно"
    if abs(worst_db) >= 10:
        return "мелочь в тихой полосе"
    return "ок"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("chip", nargs="?", help="только этот чип")
    ap.add_argument("-n", type=int, default=2, help="треков на чип")
    ap.add_argument("-t", type=float, default=3.0, help="секунд на трек")
    ap.add_argument("--timeout", type=int, default=240, help="потолок счёта на трек")
    a = ap.parse_args()

    global TRACK_TIMEOUT
    TRACK_TIMEOUT = a.timeout
    find_tools()
    if not TB.exists():
        sys.exit(f"нет стенда: {TB} — соберите sim/chipbox_tb")
    SCRATCH.mkdir(exist_ok=True)
    print(f"эталоны: vgm2wav {'найден' if VGM2WAV else 'НЕТ'}, "
          f"gbsplay {'найден' if GBSPLAY else 'НЕТ'}, "
          f"gme2wav {'найден' if GME2WAV else 'НЕТ'}\n")

    chips = [a.chip] if a.chip else list(corpus.CORPUS)
    print(f"{'чип':10} {'трек':30} {'уровень':>8} {'худшая полоса':>16} {'огиб':>6}  вывод")
    print("-" * 84)
    for chip in chips:
        picked = corpus.tracks(chip)[: a.n]
        if not picked:
            print(f"{chip:10} корпус пуст — python3 scripts/corpus.py fetch {chip}")
            continue
        for t in picked:
            ref = SCRATCH / "ref.wav"
            our = SCRATCH / "our.wav"
            for f in (ref, our):
                f.unlink(missing_ok=True)
            has_ref = render_ref(t, ref, a.t)
            if not render_ours(t, our, a.t):
                hint = ""
                if t.suffix.lower() == ".gbs":
                    hint = " — стенд собран на быстрой тактовой, для GBS нужен make CLK=57120000"
                print(f"{chip:10} {t.name[:30]:30} не уложилось в потолок{hint}", flush=True)
                continue
            ours, orate = ab.read_wav(str(our), a.t)
            o_rms = ab.rms(ours)
            if not has_ref:
                print(f"{chip:10} {t.name[:30]:30} {'—':>8} {'—':>16} {'—':>6}  "
                      f"{'нет эталона' if o_rms else 'ТИШИНА'}", flush=True)
                continue
            refs, rrate = ab.read_wav(str(ref), a.t)
            r_rms = ab.rms(refs)
            lvl = ab.db(o_rms, r_rms) if r_rms else 0.0
            re_, oe_ = ab.band_energy(refs, rrate), ab.band_energy(ours, orate)
            rs, os_ = sum(re_) or 1.0, sum(oe_) or 1.0
            worst, wband, wshare = 0.0, "", 0.0
            for (lo, hi), r, o in zip(ab.BANDS, re_, oe_):
                rp, op = r / rs, o / os_
                if max(rp, op) < 0.02:
                    continue
                d = ab.db(op, rp)
                if abs(d) > abs(worst):
                    worst, wband, wshare = d, f"{lo}-{hi}", max(rp, op)
            corr = ab.correlation(ab.envelope(refs, rrate), ab.envelope(ours, orate))
            print(f"{chip:10} {t.name[:30]:30} {lvl:+7.1f}д {wband:>9} {worst:+5.1f}д "
                  f"{corr:+6.2f}  {verdict(lvl, worst, corr, wshare)}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
