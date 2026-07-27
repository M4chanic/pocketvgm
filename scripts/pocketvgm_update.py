#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""PocketVGM updater, pupdate-style. Lives in the ROOT of the SD card as a
file named `pocketvgm` (no extension) — on macOS double-clicking opens it in
Terminal; or run manually: python3 pocketvgm. Downloads the latest release of
M4chanic/pocketvgm and unpacks it over the card (Cores/, Assets/,
Platforms/). Your music in Assets/pocketvgm/common is kept (release files are
overwritten, everything else is left alone).

The repository is public — no token needed. If one is provided (GH_TOKEN /
GITHUB_TOKEN environment variable, or a pocketvgm_token.txt file next to the
script) it is used, which helps against GitHub API rate limits.

Options: --force — reinstall even if the version matches;
         --tag vX.Y.Z — install a specific release instead of the latest.
"""

import argparse
import io
import json
import os
import ssl
import sys
import urllib.error
import urllib.request
import zipfile

REPO = "M4chanic/pocketvgm"
API = "https://api.github.com/repos/" + REPO
ROOT = os.path.dirname(os.path.abspath(__file__))
TOKEN_FILES = (os.path.join(ROOT, "pocketvgm_token.txt"),
               os.path.join(ROOT, "m4pocket_token.txt"))     # old name
VERSION_FILE = os.path.join(ROOT, ".pocketvgm_version")
OLD_VERSION_FILE = os.path.join(ROOT, ".m4pocket_version")   # old name


class _StripAuthRedirect(urllib.request.HTTPRedirectHandler):
    """GitHub serves assets via a redirect to S3; S3 rejects the request if
    the Authorization header is still present. Drop it on redirect."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is not None:
            new.remove_header("Authorization")
        return new


def _ssl_context(insecure=False):
    """CA chain: use certifi when available (framework Python on macOS has no
    system CAs until Install Certificates.command has been run)."""
    if insecure:
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        return ctx
    try:
        import certifi
        return ssl.create_default_context(cafile=certifi.where())
    except ImportError:
        return ssl.create_default_context()


_OPENER = urllib.request.build_opener(_StripAuthRedirect())


def set_opener(insecure):
    global _OPENER
    _OPENER = urllib.request.build_opener(
        urllib.request.HTTPSHandler(context=_ssl_context(insecure)),
        _StripAuthRedirect(),
    )


def http_get(url, token, accept):
    req = urllib.request.Request(url)
    req.add_header("Accept", accept)
    req.add_header("User-Agent", "pocketvgm-updater")
    if token:
        req.add_header("Authorization", "Bearer " + token)
    return _OPENER.open(req, timeout=60)


def get_token():
    """Optional token (the repo is public): helps against API rate limits."""
    for var in ("GH_TOKEN", "GITHUB_TOKEN"):
        if os.environ.get(var):
            return os.environ[var].strip(), var + " variable"
    for path in TOKEN_FILES:
        if os.path.isfile(path):
            with open(path, "r", encoding="utf-8") as f:
                tok = f.read().strip()
            if tok:
                return tok, path
    return None, None


def pick_release(token, tag):
    url = API + ("/releases/tags/" + tag if tag else "/releases/latest")
    with http_get(url, token, "application/vnd.github+json") as r:
        rel = json.load(r)
    assets = [a for a in rel.get("assets", []) if a["name"].endswith(".zip")]
    if not assets:
        raise SystemExit("Release %s has no zip asset" % rel.get("tag_name"))
    return rel["tag_name"], assets[0]


def installed_version():
    for path in (VERSION_FILE, OLD_VERSION_FILE):
        try:
            with open(path, "r", encoding="utf-8") as f:
                return f.read().strip()
        except OSError:
            continue
    return None


def download(asset, token):
    print("Downloading %s (%.1f MB)..." % (asset["name"], asset["size"] / 1e6))
    # asset url via the API + Accept: octet-stream also works for private repos
    with http_get(asset["url"], token, "application/octet-stream") as r:
        return r.read()


def extract(data):
    n = 0
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        for m in z.infolist():
            name = m.filename
            if name.startswith("/") or ".." in name.replace("\\", "/").split("/"):
                raise SystemExit("Suspicious path in archive: " + name)
            z.extract(m, ROOT)
            if not name.endswith("/"):
                n += 1
    return n


def migrate_old_core():
    """Migrate from the old core name agg23.RISCV/riscv -> M4chanic.PocketVGM."""
    old_core = os.path.join(ROOT, "Cores", "agg23.RISCV")
    old_assets = os.path.join(ROOT, "Assets", "riscv", "common")
    if not os.path.isdir(old_core) and not os.path.isdir(old_assets):
        return
    print("Found the old core agg23.RISCV (before the rename to M4chanic.PocketVGM).")
    ans = input("Move music to Assets/pocketvgm/common and remove the old core? [y/N]: ")
    if not ans.strip().lower().startswith("y"):
        print("Left as is; the music can be moved manually.")
        return
    import shutil
    new_assets = os.path.join(ROOT, "Assets", "pocketvgm", "common")
    os.makedirs(new_assets, exist_ok=True)
    if os.path.isdir(old_assets):
        for name in os.listdir(old_assets):
            src = os.path.join(old_assets, name)
            dst = os.path.join(new_assets, name)
            if name == "boot.bin" or os.path.exists(dst):
                continue
            shutil.move(src, dst)
        shutil.rmtree(os.path.join(ROOT, "Assets", "riscv"), ignore_errors=True)
    shutil.rmtree(old_core, ignore_errors=True)
    for p in (os.path.join(ROOT, "Platforms", "riscv.json"),
              os.path.join(ROOT, "Platforms", "_images", "riscv.bin")):
        try:
            os.remove(p)
        except OSError:
            pass
    print("Migration done: music in Assets/pocketvgm/common, old core removed.")


MUSIC_DIR = os.path.join(ROOT, "Assets", "pocketvgm", "common")
PLAYABLE = (".vgm", ".vgz", ".gym", ".nsf", ".gbs", ".sid", ".mid")


def _natural(name):
    """Sort key that puts 9 before 10 even without zero padding."""
    out, digits = [], ""
    for ch in name.lower():
        if ch.isdigit():
            digits += ch
        else:
            if digits:
                out.append((1, int(digits), ""))
                digits = ""
            out.append((0, 0, ch))
    if digits:
        out.append((1, int(digits), ""))
    return out


def make_playlists(quiet=False):
    """Write playlist.m3u into every music folder that has none.

    The core cannot do this itself: APF gives a core nine commands and
    none of them lists a directory, so a core only ever learns the one
    path the user picked. It does look for playlist.m3u next to an
    opened track, so writing that file here is what makes left/right and
    the Select browser work on a folder of loose files.

    A folder that already holds any .m3u is left alone — that playlist
    is someone's own ordering and outranks a generated one.
    """
    if not os.path.isdir(MUSIC_DIR):
        return 0
    written = 0
    for folder, _dirs, files in os.walk(MUSIC_DIR):
        if any(f.lower().endswith(".m3u") for f in files):
            continue
        tunes = sorted((f for f in files if f.lower().endswith(PLAYABLE)),
                       key=_natural)
        if len(tunes) < 2:
            continue          # одиночному файлу список ни к чему
        path = os.path.join(folder, "playlist.m3u")
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write("\n".join(tunes) + "\n")
        written += 1
        if not quiet:
            rel = os.path.relpath(path, ROOT)
            print("  playlist: %s (%d tracks)" % (rel, len(tunes)))
    return written


def main():
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    ap = argparse.ArgumentParser(description="Update the PocketVGM core on the SD card")
    ap.add_argument("--force", action="store_true",
                    help="reinstall even if the version matches")
    ap.add_argument("--tag", help="specific release (vX.Y.Z) instead of the latest")
    ap.add_argument("--insecure", action="store_true",
                    help="skip TLS certificate verification (when macOS CAs are not set up)")
    ap.add_argument("--playlists", action="store_true",
                    help="only write playlist.m3u into music folders, do not update")
    args = ap.parse_args()
    if args.playlists:
        n = make_playlists()
        print("Playlists written: %d" % n if n else
              "Nothing to do: every folder already has a playlist or one tune.")
        return
    set_opener(args.insecure)

    token, source = get_token()
    if token:
        print("Token: %s...%s (%s)" % (token[:8], token[-4:], source))

    try:
        tag, asset = pick_release(token, args.tag)
    except urllib.error.URLError as e:
        if "CERTIFICATE_VERIFY_FAILED" in str(e):
            raise SystemExit(
                "Python TLS certificates are not set up (typical for the "
                "python.org build on macOS).\nOptions:\n"
                "  1) run: /Applications/Python*/Install Certificates.command\n"
                "  2) pip3 install certifi\n"
                "  3) retry with --insecure (no certificate verification)")
        raise
    except urllib.error.HTTPError as e:
        if e.code == 401:
            raise SystemExit("GitHub replied 401: the token is invalid "
                             "(typo, revoked or expired).")
        if e.code == 403:
            raise SystemExit("GitHub replied 403 — likely an API rate limit.\n"
                             "Wait a bit, or put a GitHub token into "
                             "pocketvgm_token.txt (any token, no special scopes).")
        if e.code == 404:
            raise SystemExit("GitHub replied 404 — release/repository not found.")
        raise

    cur = installed_version()
    print("Installed: %s, available: %s" % (cur or "nothing", tag))
    if cur == tag and not args.force:
        print("Already up to date. Use --force to reinstall.")
        return

    n = extract(download(asset, token))
    with open(VERSION_FILE, "w", encoding="utf-8") as f:
        f.write(tag + "\n")
    try:
        os.remove(OLD_VERSION_FILE)
    except OSError:
        pass
    migrate_old_core()
    made = make_playlists()
    print("Done: %s, files extracted: %d, card: %s" % (tag, n, ROOT))
    if made:
        print("Playlists written: %d — open any tune and D-pad left/right "
              "walks its folder." % made)
    print("Insert the SD card into the Pocket — core M4chanic.PocketVGM, "
          "music in Assets/pocketvgm/common.")


if __name__ == "__main__":
    main()
