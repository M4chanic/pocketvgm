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


def _sat(frame, assign):
    """Компилируется ли участок с таким стеком при данном наборе гейтов."""
    for lit in frame:
        want = not lit.startswith("!")
        name = lit if want else lit[1:]
        if assign.get(name, False) != want:
            return False
    return True


def check_gated(path: Path) -> list[str]:
    """Сигналы, объявленные под `ifdef, но используемые там, где их нет.

    Аркадный вариант собирается без M4_HAS_HOME. Если объявление сидит под
    этим гейтом, а ссылка на него — нет, Quartus падает: в проекте задан
    default_nettype none, и неявный провод заводить нельзя. Verilator это
    НЕ ловит — директива живёт в настройках Quartus, а не в исходнике, и
    он молча создаёт провод. Так упала сборка на cen_rf5c.

    Проверка честная, а не по внутреннему гейту: гейтов в файле единицы,
    поэтому перебираются ВСЕ их сочетания. Ссылка считается опасной, если
    существует набор, при котором она компилируется, а ни одно объявление
    нет. Это разом покрывает и вложенные `ifdef, и объявления, сделанные
    в обеих ветках `ifdef/`else.

    Первая версия смотрела только на использование ВНЕ гейтов и потому
    пропустила mono_en: он был объявлен под M4_HAS_HOME, а ветка `else
    того же ifdef на него ссылалась. Аркадная сборка 0.2.9 упала на этом.
    """
    stack: list[str] = []
    declared: dict[str, list] = {}
    used: dict[str, list] = {}
    names: set[str] = set()
    for line in path.read_text(encoding="utf-8", errors="replace").split("\n"):
        t = line.strip()
        if t.startswith("`ifdef") or t.startswith("`ifndef"):
            parts = t.split()
            g = parts[1] if len(parts) > 1 else "?"
            stack.append(g if t.startswith("`ifdef") else "!" + g)
            names.add(g)
            continue
        if t.startswith("`else"):
            if stack:
                cur = stack[-1]
                stack[-1] = cur[1:] if cur.startswith("!") else "!" + cur
            continue
        if t.startswith("`endif"):
            if stack:
                stack.pop()
            continue
        if t.startswith("//"):
            continue
        code = re.sub(r"//.*", "", line)
        frame = frozenset(stack)
        m = DECL.match(code)
        if m:
            # отбрасываем разрядность и тип, остаются только имена
            tail = re.sub(r"\[[^\]]*\]", "", m.group(1))
            tail = re.sub(r"\b(?:signed|unsigned)\b", "", tail)
            for nm in DECL_NAME.findall(tail):
                declared.setdefault(nm, set()).add(frame)
        # Имя порта в подключении (.cen(sig)) — не ссылка на сигнал
        for nm in WORD.findall(re.sub(r"\.\s*\w+\s*\(", "(", code)):
            used.setdefault(nm, set()).add(frame)

    # Настоящих вариантов сборки три, а не 2^n: наверху chipbox.sv стоит
    # `ifdef M4_SIM -> define HOME и ARCADE, `elsif M4_ARCADE -> ARCADE,
    # иначе HOME. Перебирать сочетания вслепую значило бы ругаться на
    # заведомо невозможное «M4_SIM без M4_HAS_HOME».
    combos = [
        {"M4_SIM": True, "M4_HAS_HOME": True, "M4_HAS_ARCADE": True},
        {"M4_ARCADE": True, "M4_HAS_ARCADE": True},
        {"M4_HAS_HOME": True},
    ]
    gates = sorted(names)

    out = []
    for name, decls in sorted(declared.items()):
        bad = None
        for u in used.get(name, ()):  # noqa: B007
            for a in combos:
                if _sat(u, a) and not any(_sat(d, a) for d in decls):
                    bad = (u, a)
                    break
            if bad:
                break
        if not bad:
            continue
        u, a = bad
        on = ", ".join(g for g in gates if a.get(g, False)) or "ничего"
        where = "вне гейтов" if not u else "под " + ", ".join(sorted(u))
        out.append(f"{path}: '{name}' используется {where}, "
                   f"но при наборе [{on}] не объявлен")
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
