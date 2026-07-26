#!/usr/bin/env python3
"""Сборка релизного архива из артефактов CI.

Фирмварь и битстримы собираются на GitHub Actions, а не здесь. Локально
boot.bin побайтно не повторить: CI берёт свой nightly, и код ложится
иначе — сборка из того же коммита отличалась на 316 байт. Из-за этого в
0.2.0 уехала фирмварь предыдущего релиза: архив собирался руками, и
никто не сверял, откуда взялся boot.bin.

Скрипт берёт артефакты конкретного прогона CI, раскладывает их так же,
как это делает джоб package, проверяет пакет через check_package.py и
печатает контрольные суммы — чтобы было видно, что именно уходит.

    python3 scripts/make_release.py 0.2.1                 последний удачный прогон main
    python3 scripts/make_release.py 0.2.1 --run 30223413276
    python3 scripts/make_release.py 0.2.1 --out /tmp/rel

Публикацию скрипт не делает: тег и gh release create — отдельный шаг,
осознанный.
"""

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPO = "M4chanic/pocketvgm"


def gh(*args, **kw):
    return subprocess.run(["gh", *args], capture_output=True, text=True,
                          check=True, **kw).stdout


def latest_run():
    """Последний удачный прогон на main, где есть оба артефакта."""
    runs = json.loads(gh("run", "list", "--repo", REPO, "--branch", "main",
                         "--status", "success", "--limit", "10",
                         "--json", "databaseId,headSha,displayTitle"))
    for r in runs:
        names = json.loads(gh("api", f"repos/{REPO}/actions/runs/"
                              f"{r['databaseId']}/artifacts",
                              "--jq", "[.artifacts[].name]"))
        if {"bitstream", "firmware"} <= set(names):
            return r
    sys.exit("не нашёл прогона, где есть и bitstream, и firmware")


def md5(path):
    h = hashlib.md5()
    h.update(path.read_bytes())
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("version", help="версия, например 0.2.1")
    ap.add_argument("--run", help="id прогона CI; по умолчанию последний удачный")
    ap.add_argument("--out", default=None, help="куда складывать (по умолчанию временный каталог)")
    a = ap.parse_args()

    if a.run:
        run_id, title = a.run, "(задан вручную)"
    else:
        r = latest_run()
        run_id, title = str(r["databaseId"]), f"{r['headSha'][:7]} {r['displayTitle']}"
    print(f"прогон CI: {run_id} {title}", flush=True)

    out = Path(a.out).resolve() if a.out else Path(tempfile.gettempdir()) / f"pocketvgm-release-{a.version}"
    art, dist = out / "artifacts", out / "dist"
    for d in (art, dist):
        shutil.rmtree(d, ignore_errors=True)
    art.mkdir(parents=True)

    for name in ("bitstream", "firmware"):
        gh("run", "download", run_id, "--repo", REPO, "-n", name, "-D", str(art))

    # Раскладка ровно как в джобе package
    shutil.copytree(ROOT / "core" / "pkg" / "pocket", dist)
    cores = dist / "Cores"
    for rev in art.glob("pocketvgm*.rev"):
        shutil.copy(rev, cores / "M4chanic.PocketVGM" / rev.name)
    shutil.copy(art / "pocketvgm_a.rev",
                cores / "M4chanic.PocketVGMArcade" / "arcade.rev")
    common = dist / "Assets" / "pocketvgm" / "common"
    common.mkdir(parents=True, exist_ok=True)
    shutil.copy(art / "boot.bin", common / "boot.bin")
    if (common / "Demo").exists():
        shutil.rmtree(common / "Demo")
    shutil.copytree(ROOT / "Demo", common / "Demo")
    upd = dist / "pocketvgm"
    shutil.copy(ROOT / "scripts" / "pocketvgm_update.py", upd)
    upd.chmod(0o755)

    # Версия в core.json должна совпадать с тем, что просили
    for cj in sorted(cores.glob("*/core.json")):
        v = json.loads(cj.read_text())["core"]["metadata"]["version"]
        mark = "ок" if v == a.version else "НЕ СОВПАДАЕТ"
        print(f"  {cj.parent.name:26} version {v}  {mark}", flush=True)
        if v != a.version:
            sys.exit(f"поправьте version в {cj.relative_to(ROOT)} и повторите")

    if subprocess.run([sys.executable, str(ROOT / "scripts" / "check_package.py"),
                       str(dist)]).returncode:
        sys.exit("пакет не прошёл проверку")

    zip_path = out / f"pocketvgm-v{a.version}.zip"
    zip_path.unlink(missing_ok=True)
    # апдейтер лежит в корне карты рядом с Cores — README обещает его
    # в архиве, а собранные руками релизы его теряли
    subprocess.run(["zip", "-qr", str(zip_path),
                    "Cores", "Assets", "Platforms", "pocketvgm"],
                   cwd=dist, check=True)

    print("\nчто уходит:")
    for f in sorted(dist.rglob("*.rev")) + [common / "boot.bin"]:
        print(f"  {md5(f)}  {f.relative_to(dist)}")
    print(f"\n{zip_path}  ({zip_path.stat().st_size} байт)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
