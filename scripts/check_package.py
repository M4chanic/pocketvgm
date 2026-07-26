#!/usr/bin/env python3
"""Проверка пакета ядра по спецификации openFPGA — до того, как он уедет.

Ядро аркад не грузилось на устройстве («Load error in "core"», затем
«Error in core setup») из-за того, что в его папке не было variants.json:
документация числит его среди семи обязательных файлов. Рядом нашлись ещё
три отклонения — пробел в shortname, description длиннее предела и имя
битстрима ровно в предельные 15 символов. Каждое из них видно статически,
и ни одно не видно на глаз.

Проверяется:
  наличие семи обязательных json в каждой папке ядра;
  что каждый файл из cores[] лежит рядом;
  пределы длин полей метаданных, имён и filename;
  что папка называется author.shortname;
  что обязательные слоты данных находят свой файл в Assets.

    python3 scripts/check_package.py dist        каталог пакета
    python3 scripts/check_package.py core/pkg/pocket   без битстримов
"""

import json
import sys
from pathlib import Path

# Пределы из документации Analogue по core.json
LIMITS = {"shortname": 31, "description": 63, "author": 31,
          "url": 63, "version": 31, "date_release": 10}
REQUIRED_JSON = ("core.json", "audio.json", "data.json", "input.json",
                 "interact.json", "variants.json", "video.json")


def check_core(folder, root, bad, full):
    """folder — Cores/Author.Name, root — корень пакета.

    full — в пакете есть битстримы, значит они обязаны быть у каждого
    ядра. Признак считается по всему пакету: если смотреть только на
    свою папку, ядро без единого битстрима само себя и оправдает —
    ровно так первая версия этой проверки пропустила пакет, где вся
    папка аркад осталась без .rev.
    """
    where = folder.name

    for name in REQUIRED_JSON:
        if not (folder / name).is_file():
            bad.append(f"{where}: нет обязательного {name}")

    cj = folder / "core.json"
    if not cj.is_file():
        return
    try:
        core = json.loads(cj.read_text())["core"]
    except Exception as e:
        bad.append(f"{where}: core.json не читается — {e}")
        return

    meta = core.get("metadata", {})
    for field, limit in LIMITS.items():
        v = meta.get(field, "")
        if len(v) > limit:
            bad.append(f"{where}: {field} — {len(v)} символов при пределе {limit}")

    author, short = meta.get("author", ""), meta.get("shortname", "")
    if where != f"{author}.{short}":
        bad.append(f"{where}: имя папки не совпадает с author.shortname "
                   f"({author}.{short})")

    entries = core.get("cores", [])
    if not 1 <= len(entries) <= 8:
        bad.append(f"{where}: в cores[] {len(entries)} элементов, допустимо 1..8")
    for c in entries:
        fn = c.get("filename", "")
        if len(fn) > 15:
            bad.append(f"{where}: filename {fn!r} — {len(fn)} символов при пределе 15")
        if len(c.get("name", "")) > 15:
            bad.append(f"{where}: name {c.get('name')!r} длиннее 15 символов")
        if full and not (folder / fn).is_file():
            bad.append(f"{where}: cores[] ссылается на {fn}, а файла нет")

    platforms = meta.get("platform_ids", [])
    if not 1 <= len(platforms) <= 4:
        bad.append(f"{where}: platform_ids — {len(platforms)} штук, допустимо 1..4")

    dj = folder / "data.json"
    # слоты проверяем только у полного пакета: в репозитории Assets ещё
    # пуст, туда всё складывается при упаковке
    if not full or not dj.is_file() or not platforms:
        return
    try:
        slots = json.loads(dj.read_text())["data"]["data_slots"]
    except Exception as e:
        bad.append(f"{where}: data.json не читается — {e}")
        return
    for s in slots:
        if not s.get("required"):
            continue
        fn = s.get("filename")
        if not fn:
            bad.append(f"{where}: слот {s.get('name')!r} обязателен, но без filename")
            continue
        # бит 1 параметров: файл принадлежит ядру, иначе платформе
        par = int(str(s.get("parameters", "0")), 0)
        base = root / "Assets" / platforms[0]
        path = base / where / fn if par & 2 else base / "common" / fn
        if not path.is_file():
            bad.append(f"{where}: слот {s.get('name')!r} не находит {path.relative_to(root)}")


def main():
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "dist").resolve()
    cores = root / "Cores"
    if not cores.is_dir():
        sys.exit(f"нет каталога {cores}")
    bad = []
    folders = sorted(p for p in cores.iterdir() if p.is_dir())
    if not folders:
        sys.exit(f"в {cores} нет ни одной папки ядра")
    # битстримов нет в репозитории, они приезжают из CI: полным пакетом
    # считаем тот, где .rev нашёлся хоть у одного ядра
    full = any(any(f.glob("*.rev")) for f in folders)
    for f in folders:
        check_core(f, root, bad, full)
    for line in bad:
        print("ОШИБКА", line)
    print(f"\nпроверено ядер: {len(folders)}, замечаний: {len(bad)}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
