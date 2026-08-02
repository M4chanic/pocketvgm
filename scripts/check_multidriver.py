#!/usr/bin/env python3
"""Проверки собственного RTL, которых не делает Verilator.

Ищет регистры, которым присваивают в двух разных always-блоках.

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
# Присваивание в начале строки ЛИБО сразу после метки case: строка вида
# «EXT_RF5C_PTR: rf5c_ram_ptr <= ...» раньше не опознавалась, и из-за
# этого двойной драйвер на rf5c_ram_ptr дошёл до Quartus.
ASSIGN = re.compile(
    r"^[ \t]*(?:[A-Za-z_0-9][\w']*(?:\[[^\]]*\])?\s*:\s*)?"
    r"([A-Za-z_]\w*)\s*(?:\[[^\]]*\])*\s*<=", re.M)

# Знаковый литерал, не влезающий в свою же разрядность: 16'sd32768 при
# знаковых 16 битах даёт максимум 32767. Quartus пишет «constant value
# overflow», Verilator молчит, а значение получается верным только по
# случайности — два таких literal-а прожили до самого битстрима.
SIGNED_LIT = re.compile(r"(\d+)'s([dhb])([0-9a-fA-F_]+)", re.I)


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


# Объявление целиком, вместе со списком имён через запятую: строка
# «reg signed [25:0] ym_l_g, ym_r_g, adpcm_l_g;» объявляет три сигнала, а
# не один — иначе второй и третий выглядят необъявленными.
DECL = re.compile(r"^[ \t]*(?:reg|wire|logic|integer|localparam)\b(.*)$")
DECL_NAME = re.compile(r"\b([A-Za-z_]\w*)\s*(?:=[^,;]*)?(?=\s*[,;]|\s*$)")
WORD = re.compile(r"\b([A-Za-z_]\w*)\b")


def check_gated(path: Path) -> list[str]:
    """Сигналы, объявленные под `ifdef, но используемые снаружи.

    Аркадный вариант собирается без M4_HAS_HOME. Если объявление сидит под
    этим гейтом, а ссылка на него — нет, Quartus падает: в проекте задан
    default_nettype none, и неявный провод заводить нельзя. Verilator это
    НЕ ловит — директива живёт в настройках Quartus, а не в исходнике, и
    он молча создаёт провод. Так упала сборка на cen_rf5c.
    """
    stack: list[str] = []
    declared: dict[str, set] = {}   # имя -> множество гейтов объявления
    used_open: set[str] = set()     # имена, встреченные ВНЕ гейтов
    for line in path.read_text(encoding="utf-8", errors="replace").split("\n"):
        t = line.strip()
        if t.startswith("`ifdef") or t.startswith("`ifndef"):
            parts = t.split()
            stack.append(parts[1] if len(parts) > 1 else "?")
            continue
        if t.startswith("`else"):
            if stack:
                stack[-1] = "!" + stack[-1]
            continue
        if t.startswith("`endif"):
            if stack:
                stack.pop()
            continue
        if t.startswith("//"):
            continue
        code = re.sub(r"//.*", "", line)
        m = DECL.match(code)
        if m:
            # отбрасываем разрядность и тип, остаются только имена
            tail = re.sub(r"\[[^\]]*\]", "", m.group(1))
            tail = re.sub(r"\b(?:signed|unsigned)\b", "", tail)
            for nm in DECL_NAME.findall(tail):
                declared.setdefault(nm, set()).add(stack[-1] if stack else "")
        if not stack:
            # Имя порта в подключении (.cen(sig)) — не ссылка на сигнал
            used_open.update(WORD.findall(re.sub(r"\.\s*\w+\s*\(", "(", code)))

    out = []
    for name, gates in sorted(declared.items()):
        if name not in used_open:
            continue
        # объявлено где-то без гейта, либо и под G, и под !G — тогда есть всегда
        if "" in gates or any(g.startswith("!") and g[1:] in gates for g in gates):
            continue
        gate = sorted(gates)[0]
        out.append(f"{path}: '{name}' объявлен только под `ifdef {gate}, "
                   f"а используется вне гейта")
    return out


def check_signed_lits(path: Path) -> list:
    """Знаковые литералы, переполняющие собственную разрядность."""
    src = strip_comments(path.read_text())
    base = {"d": 10, "h": 16, "b": 2}
    out = []
    for m in SIGNED_LIT.finditer(src):
        width = int(m.group(1))
        try:
            val = int(m.group(3).replace("_", ""), base[m.group(2).lower()])
        except ValueError:
            continue
        if width == 0 or width > 63:
            continue
        # Десятичный знаковый литерал задаёт ЧИСЛО, и оно обязано влезть в
        # знаковый диапазон: 16'sd32768 при максимуме 32767 переполняется.
        # Шестнадцатеричный и двоичный задают битовую КАРТИНУ той же
        # разрядности, поэтому 16'sh8000 — законная запись для -32768, и
        # ругаться на неё нельзя: она у нас уже используется как верная.
        limit = (1 << (width - 1)) if m.group(2).lower() == "d" else (1 << width)
        if val >= limit:
            line = src[: m.start()].count("\n") + 1
            out.append(f"{path}:{line}: {m.group(0)} не влезает в знаковые "
                       f"{width} бит (предел {limit - 1})")
    return out


def main() -> int:
    files = [Path(p) for p in (sys.argv[1:] or DEFAULT)]
    problems = []
    gate_problems = []
    lit_problems = []
    for f in files:
        if f.exists():
            problems += check(f)
            gate_problems += check_gated(f)
            lit_problems.extend(check_signed_lits(f))
    if problems:
        print("несколько драйверов на регистр (Quartus это отвергнет):")
        for p in problems:
            print("  " + p)
        return 1
    if gate_problems:
        print("объявлено под `ifdef, а используется снаружи "
              "(аркадный вариант не соберётся):")
        for p in gate_problems:
            print("  " + p)
        return 1
    if lit_problems:
        print("знаковые литералы переполняют разрядность (Quartus: constant value overflow):")
        for p in lit_problems:
            print("  " + p)
        return 1
    print(f"драйверы, гейты и литералы: чисто ({len(files)} файлов)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
