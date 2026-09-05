# PocketVGM — chiptune player core for Analogue Pocket (openFPGA)

An openFPGA core that plays chiptune formats through FPGA simulation of the
original sound chips (Cyclone V 5CEBA4F23C8). The control platform is the
RISC-V SoC [agg23/openfpga-litex](https://github.com/agg23/openfpga-litex);
the player and UI are written in Rust.

Prebuilt releases: [Releases](https://github.com/M4chanic/pocketvgm/releases).

## Formats

Two Load menu entries, split by what the file is:

| Menu | Extensions | Content |
|---|---|---|
| VGM & playlists | `.m3u` `.vgm` `.vgz` `.gym` | playlists, register logs of any logged system, Genesis logs |
| NSF & GBS | `.vgm` `.vgz` `.nsf` `.gbs` | register logs, NES/Famicom music, Game Boy music |

Commodore 64 `.sid` and General MIDI `.mid` are not in the menu: the core
is aimed at consoles, and the SID took nine percent of the FPGA that the
console chips need. Both can come back — MIDI needs no hardware of its
own, the SID needs the core rebuilt with `M4_HAS_SID`.

The Arcade core has a single entry, **Load Arcade**, taking `.vgm` `.vgz`
`.m3u` — the arcade chips are only ever logged in those formats, so there
is nothing to group.

A playlist opened from any menu can list tracks of any supported format —
the player reads the `.m3u` and then opens each track by path.

Opening a single tune also gets a playlist, if one sits beside it: the core
looks for `playlist.m3u` in the same folder, then for a file named after the
folder, and starts that list at the tune you picked. So D-pad left/right and
the Select browser walk the whole folder rather than the one file. A core
cannot build that list on its own — of the nine commands APF gives it, none
lists a directory, and a core only ever learns the one path the user picked.
The updater writes the missing `playlist.m3u` files for you; see below.

VGM/VGZ appear in both menus — packs for any platform may come in these
formats. Multi-song files (NSF, GBS) switch subsongs with the D-pad.

## Supported systems

What the core plays, by machine. Everything in the first group works in the
default Console bitstream; the second needs the Arcade one.

| System | Formats | Chips used |
|---|---|---|
| Sega Mega Drive / Genesis | `.vgm` `.vgz` `.gym` | YM2612 + SN76489 |
| Sega 32X | `.vgm` `.vgz` | YM2612 + SN76489 + PWM |
| Sega Mega CD | `.vgm` `.vgz` | YM2612 + SN76489 + RF5C164 |
| Sega Master System, Game Gear | `.vgm` `.vgz` | SN76489 |
| NES / Famicom | `.nsf` `.vgm` | NES APU + DMC, VRC6, VRC7, Sunsoft 5B |
| Game Boy, Game Boy Color | `.gbs` `.vgm` `.vgz` | SM83 + Game Boy APU |
| PC Engine / TurboGrafx-16 | `.vgm` `.vgz` | HuC6280 |
| Vectrex | `.vgm` `.vgz` | AY-3-8912 |
| MSX | `.vgm` `.vgz` | AY-3-8910, Konami SCC |
| NEC PC-88, PC-98 | `.vgm` `.vgz` | YM2203, YM2608 (melodic part) |
| ZX Spectrum | `.vgm` `.vgz` | AY-3-8910 |

| In the separate Arcade core | Formats | Chips used |
|---|---|---|
| Sharp X68000 | `.vgm` `.vgz` | YM2151, MSM6258 |
| Sega arcade (OutRun and similar) | `.vgm` `.vgz` | YM2151, SegaPCM |
| Konami arcade | `.vgm` `.vgz` | K053260 (2 of 4 channels) |
| Arcade boards using OKIM6295 | `.vgm` `.vgz` | OKIM6295 |

These live in a second core, **PocketVGM Arcade**, installed alongside the
first and listed separately in the Pocket menu. Both share the same
platform and the same music folder, so nothing needs copying twice.

Two sets rather than one because they do not fit on the device together —
the chip logic of both exceeds what the FPGA holds. They were originally
meant to be two variants of one core, selected from a menu, but that
mechanism is documented as upcoming and no shipping core uses it: of 260
repositories carrying a `variants.json`, every one has an empty list. Two
cores is what works today.

A recording written for the console chips is silent in the Arcade core and
the reverse is also true, and nothing yet tells you which one a file
needs.

Two parts of the YM2608 are not modelled: the rhythm section, whose samples
live in a ROM inside the original chip, and ADPCM-B. Melodic FM and SSG
play in full.

## Simulated chips

| Chip | Found in | RTL |
|---|---|---|
| YM2612 (OPN2) | Sega Mega Drive / Genesis | [jt12](https://github.com/jotego/jt12) |
| YM2203 (OPN), YM2608 (OPNA) | NEC PC-88 / PC-98 | routed onto the YM2612 and AY above |
| SN76489 (PSG) | Master System, Game Gear, Mega Drive | [jt89](https://github.com/jotego/jt89) |
| YM2151 (OPM) | Sega/Konami arcade, Sharp X68000 | [jt51](https://github.com/jotego/jt51) |
| SegaPCM, MSM6258 | Sega arcade (OutRun etc.), X68000 | jtoutrun / custom |
| AY-3-8910 / YM2149 / Sunsoft 5B | MSX, ZX Spectrum, Famicom (5B) | [jt49](https://github.com/jotego/jt49) |
| NES APU + DMC, VRC6 | NES / Famicom | [NES_MiSTer](https://github.com/MiSTer-devel/NES_MiSTer) / custom |
| Game Boy APU (+ SM83 CPU) | Game Boy (GBS) | [VerilogBoy](https://github.com/zephray/VerilogBoy) |
| HuC6280 PSG | PC Engine / TurboGrafx-16 | custom, written for this project |
| Konami SCC (K051649) | MSX cartridges | [IKASCC](https://github.com/ika-musume/IKASCC) |
| OKIM6295 | arcade | [jt6295](https://github.com/jotego/jt6295) |
| K053260 | Konami arcade | [jtcores](https://github.com/jotego/jtcores) (2 of 4 channels, for area) |
| YMF262 (OPL3, OPL2 subset) | PC AdLib logs, YM2413/VRC7 through a register translator | [opl3_fpga](https://github.com/gtaylormb/opl3_fpga) |
| YM2413 (OPLL) / VRC7 | MSX-Music, NES (Lagrange Point) | register translation onto opl3_fpga, patch ROM from libvgm |

Upstream sources and commits of the vendored RTL are listed in
[`rtl/vendor/VENDOR.md`](rtl/vendor/VENDOR.md).

## Installation

1. Download the zip from [Releases](https://github.com/M4chanic/pocketvgm/releases)
   and unpack it into the root of the SD card (the `Cores/`, `Platforms/` etc.
   folders merge with the existing ones).
2. Put music into `Assets/pocketvgm/common/` on the card (recommended
   location) and open it through the core's Load menus. A `Demo/` folder
   with freely licensed music in all supported formats is included there.

For updates there is the `pocketvgm` updater script (Python 3, included in
the release zip): run it by double-clicking on macOS or with
`python3 pocketvgm` — it finds the SD card, downloads the latest release and
updates the core files.

It also writes a `playlist.m3u` into every music folder that has none, which
is what lets left/right walk a folder of loose files. Run `python3 pocketvgm
--playlists` after adding music to refresh those lists without updating the
core. A folder that already contains any `.m3u` is left alone — that ordering
is yours and outranks a generated one.

## Controls

- **Left / Right** — previous/next track (or subsong within NSF/GBS)
- **A** — pause, **B** — stop
- **R (hold)** — fast forward ×8
- **Select** — playlist browser

On start the core shows a title screen and waits for **A**, unless a file was
just picked through Load — then it plays straight away.

## Repository layout

- `core/` — the openFPGA core: `core_top.sv`, `chipbox.sv` (chip bus),
  Quartus project, `pkg/pocket/` — SD card files (json, assets)
- `core/lang/rust/examples/player/` — the Rust player/UI (RISC-V soft core)
- `rtl/chipbox/` — custom modules: GBS box (SM83+APU), VRC6, MSM6258, OPLL-to-OPL2 translator
- `rtl/vendor/` — vendored chip RTL (see `VENDOR.md`, mostly GPL-3.0)
- `firmware/vgm-core/` — shared Rust parser library (format parsers,
  inflate, MD5 for HVSC song lengths)
- `sim/` — Verilator harnesses: `vgmplay` (VGM→WAV on a PC), `chipbox_tb`
  (self-tests of the whole path, including real NSF and GBS files)
- `scripts/` — updater, artwork generation, demo tune generators,
  VGM→GYM converter; `check_package.py` validates a built package against
  the openFPGA spec and `make_release.py` assembles one from CI artifacts;
  the sound comparison tools are `ab_compare.py` and `ab_suite.py`, which
  measure our rendering against a reference emulator by level, band
  balance and envelope, `tone_check.py`, which calibrates pitch on a
  single synthetic note, and `corpus.py`, which fetches test material
- `.github/workflows/` — CI: simulation + self-tests on every push, Quartus
  21.1 bitstream in Docker (seed matrix with best-slack pick on
  workflow_dispatch)

## Building from source

PC simulation (Verilator ≥ 5):

```sh
cd sim/vgmplay && make
./vgmplay path/to/song.vgz -o out.wav -t 20   # -t N — first N seconds

cd sim/chipbox_tb && make && ./chipbox_tb      # path self-tests
```

Bitstream — Quartus 21.1 (Docker image `raetro/quartus:21.1`); firmware —
Rust nightly for `riscv32imac` (soft float: the FPU was removed to free
logic). The easiest reference is [`.github/workflows/build.yml`](.github/workflows/build.yml).

Released packages are assembled from CI artifacts by
`scripts/make_release.py`, not from a local build. A local firmware build
does not reproduce the released `boot.bin` byte for byte — the toolchain
nightly differs and the code lays out differently — so mixing the two is
how a release ends up carrying a firmware nobody chose.

## Code sources and acknowledgements

The chip RTL is vendored from open projects (exact commits and local patches
are listed in [`rtl/vendor/VENDOR.md`](rtl/vendor/VENDOR.md)):

- [jotego/jtcores](https://github.com/jotego) — jt51, jt12, jt89, jt49,
  jtoutrun (SegaPCM); Jose Tejada's FM/PSG core library
- [MiSTer-devel](https://github.com/MiSTer-devel) — NES APU
  ([NES_MiSTer](https://github.com/MiSTer-devel/NES_MiSTer)) and SID
  ([C64_MiSTer](https://github.com/MiSTer-devel/C64_MiSTer), DAC/filters
  based on Dag Lem's reSID/reDIP-SID)
- [zephray/VerilogBoy](https://github.com/zephray/VerilogBoy) — SM83 CPU and
  Game Boy APU
- [Arlet/verilog-6502](https://github.com/Arlet/verilog-6502) — 6502 for NSF
- [gtaylormb/opl3_fpga](https://github.com/gtaylormb/opl3_fpga) — YMF262 (OPL3)
- [libvgm](https://github.com/ValleyBell/libvgm) — the reference renderer this
  project measures against, and the source of the 32X PWM scaling (Gens/GS)
- [agg23/openfpga-litex](https://github.com/agg23/openfpga-litex) — base core:
  RISC-V SoC, APF bridge, openFPGA infrastructure
- [freedoom](https://github.com/freedoom/freedoom) — GENMIDI GM patches for
  MIDI synthesis; [dhepper/font8x8](https://github.com/dhepper/font8x8) — UI font

## License

Own code — [GPL-3.0](LICENSE) (dictated by the vendored jt51/jt12/NES/SID RTL).
Third-party module licenses are in their directories and in
`rtl/vendor/VENDOR.md`.
