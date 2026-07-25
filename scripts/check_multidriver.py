#!/usr/bin/env python3
"""Ищет регистры, которым присваивают в двух разных always-блоках.

Quartus такое отвергает («Can't resolve multiple constant drivers»), а
Verilator для массивов молчит — из-за этого ошибка дважды всплывала
только на сборке битстрима, через сорок минут после пуша.

Проверяются только собственные файлы: вендорный код правим не мы.
Запуск: python3 scripts/check_multidriver.py [файлы...]
"""

import re
import sys
from pathlib import Path

DEFAULT = [
    "rtl/chipbox/huc6280_psg.sv",
    "rtl/chipbox/gbsbox.sv",
    "rtl/chipbox/vrc6.sv",
    "rtl/chipbox/msm6258.sv",
    "core/target/pocket/chipbox.sv",
]

# `always` внутри строки/комментария нас не интересует
ALWAYS = re.compile(r"^[ \t]*always(?:_ff|_comb|_latch)?\b", re.M)
ASSIGN = re.compile(r"^\s*([A-Za-z_]\w*)\s*(?:\[[^\]]*\])*\s*<=", re.M)


def strip_comments(text: str) -> str:
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def check(path: Path) -> list[str]:
    src = strip_comments(path.read_text(encoding="utf-8", errors="replace"))
    starts = [m.start() for m in ALWAYS.finditer(src)]
    if not starts:
        return []
    bounds = list(zip(starts, starts[1:] + [len(src)]))

    owner: dict[str, int] = {}
    bad: dict[str, set[int]] = {}
    for idx, (a, b) in enumerate(bounds):
        for m in ASSIGN.finditer(src[a:b]):
            sig = m.group(1)
            if sig in owner and owner[sig] != idx:
                bad.setdefault(sig, {owner[sig]}).add(idx)
            owner.setdefault(sig, idx)

    out = []
    for sig, blocks in sorted(bad.items()):
        line = src[: bounds[min(blocks)][0]].count("\n") + 1
        out.append(f"{path}:{line}: '{sig}' присваивается в {len(blocks)} always-блоках")
    return out


def main() -> int:
    files = [Path(p) for p in (sys.argv[1:] or DEFAULT)]
    problems = []
    for f in files:
        if f.exists():
            problems += check(f)
    if problems:
        print("несколько драйверов на регистр (Quartus это отвергнет):")
        for p in problems:
            print("  " + p)
        return 1
    print(f"драйверы: чисто ({len(files)} файлов)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
