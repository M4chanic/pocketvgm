#!/usr/bin/env python3
"""Тестовый корпус для проверки звучания: список и загрузка.

Материал — рипы с vgmrips и zophar, по нескольку на каждый чип. Он
копирайтный, поэтому живёт в Test/corpus, который в .gitignore, и никогда
не попадает ни в репозиторий, ни в релиз. Проверять надо на настоящей
музыке: за день отладки трижды выяснялось, что синтетический тест сходится
с эталоном, а реальный рип — нет.

    python3 scripts/corpus.py list          что в списке
    python3 scripts/corpus.py fetch         скачать недостающее
    python3 scripts/corpus.py fetch huc6280 только один чип
"""

import os
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEST = ROOT / "Test" / "corpus"

VGMRIPS = "https://vgmrips.net/files/"
ZOPHAR = "https://fi.zophar.net/soundfiles/"

# Чип -> список паков. Для vgmrips путь относительно files/, для zophar —
# относительно soundfiles/. Выбраны вещи с плотной аранжировкой: на них
# перекос слышен и меряется лучше, чем на разреженной музыке.
CORPUS = {
    "ym2612": [   # Mega Drive: FM + PSG
        ("vgmrips", "MegaDrive/Streets%20of%20Rage%202%20%28Bare%20Knuckle%20II%29%20%28Mega%20Drive%2C%20Genesis%29.zip"),
        ("vgmrips", "MegaDrive/Thunder%20Force%20IV%20%28Lightening%20Force%29%20%28Mega%20Drive%2C%20Genesis%29.zip"),
    ],
    "sn76489": [  # Master System
        ("vgmrips", "MasterSystem/Sonic%20The%20Hedgehog%20%28Master%20System%29.zip"),
    ],
    "huc6280": [  # PC Engine
        ("vgmrips", "TurboGrafx/Soldier_Blade_%28TG-16%29.zip"),
        ("vgmrips", "TurboGrafx/Devil_Crash_%28PC_Engine%29.zip"),
        ("vgmrips", "TurboGrafx/Magical_Chase_%28TG-16%29.zip"),
    ],
    "ym2608": [   # PC-98 / PC-88, OPNA
        ("vgmrips", "Computers/NEC/Grounseed_%28NEC_PC-9801%2C_OPNA%29.zip"),
        ("vgmrips", "Computers/NEC/The_Scheme_%28NEC_PC-8801%2C_OPNA%29.zip"),
    ],
    "ym2203": [   # PC-98, OPN
        ("vgmrips", "Computers/NEC/EVE_burst_error_%28NEC_PC-9801%29.zip"),
    ],
    "scc": [      # MSX, Konami SCC
        ("vgmrips", "Computers/MSX/Space_Manbow_%28MSX2%29.zip"),
        ("vgmrips", "Computers/MSX/Nemesis_2_%28MSX%29.zip"),
    ],
    "ay8910": [   # MSX / ZX
        ("vgmrips", "Computers/MSX/Metal_Gear_2_-_Solid_Snake_%28MSX2%29.zip"),
    ],
    "nes": [      # NSF
        ("zophar", "nintendo-nes-nsf/contra/Contra%20%28EMU%29.zophar.zip"),
        ("zophar", "nintendo-nes-nsf/final-fantasy/Final%20Fantasy%20%28EMU%29.zophar.zip"),
        ("zophar", "nintendo-nes-nsf/mega-man-2/Mega%20Man%202%20%28EMU%29.zophar.zip"),
    ],
    "gameboy": [  # GBS
        ("zophar", "gameboy-gbs/tetris/Tetris%20%28EMU%29.zophar.zip"),
        ("zophar", "gameboy-gbs/super-mario-land/Super%20Mario%20Land%20%28EMU%29.zophar.zip"),
    ],
}

EXTS = (".vgm", ".vgz", ".nsf", ".gbs", ".sid")


def fetch_one(kind, rel, chip):
    base = VGMRIPS if kind == "vgmrips" else ZOPHAR
    out = DEST / chip
    out.mkdir(parents=True, exist_ok=True)
    name = rel.rsplit("/", 1)[-1]
    zpath = out / name
    if not zpath.exists():
        r = subprocess.run(
            ["curl", "-sSL", "-A", "Mozilla/5.0", "--max-time", "120",
             "-o", str(zpath), base + rel],
            capture_output=True)
        if r.returncode != 0 or zpath.stat().st_size < 1000:
            zpath.unlink(missing_ok=True)
            return f"НЕ СКАЧАЛСЯ {name}"
    try:
        with zipfile.ZipFile(zpath) as z:
            members = [m for m in z.namelist() if m.lower().endswith(EXTS)]
            z.extractall(out, members)
        zpath.unlink()
        return f"{len(members):3d} файлов  {name[:48]}"
    except zipfile.BadZipFile:
        zpath.unlink(missing_ok=True)
        return f"НЕ АРХИВ {name}"


def tracks(chip=None):
    """Пути к музыке в корпусе."""
    root = DEST / chip if chip else DEST
    if not root.exists():
        return []
    return sorted(p for p in root.rglob("*") if p.suffix.lower() in EXTS)


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    only = sys.argv[2] if len(sys.argv) > 2 else None
    if cmd == "list":
        for chip, packs in CORPUS.items():
            have = len(tracks(chip))
            print(f"  {chip:10} {len(packs)} паков, скачано треков: {have}")
        print(f"\nкорпус: {DEST} (в .gitignore, наружу не уходит)")
    elif cmd == "fetch":
        for chip, packs in CORPUS.items():
            if only and chip != only:
                continue
            print(f"{chip}:")
            for kind, rel in packs:
                print("   ", fetch_one(kind, rel, chip))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
