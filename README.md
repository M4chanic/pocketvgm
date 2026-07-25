# PocketVGM — chiptune player core for Analogue Pocket (openFPGA)

An openFPGA core that plays chiptune formats through FPGA simulation of the
original sound chips (Cyclone V 5CEBA4F23C8). The control platform is the
RISC-V SoC [agg23/openfpga-litex](https://github.com/agg23/openfpga-litex);
the player and UI are written in Rust.

Prebuilt releases: [Releases](https://github.com/M4chanic/pocketvgm/releases).

## Formats

Three Load menu entries, grouped by platform:

| Menu | Extensions | Content |
|---|---|---|
| Sega | `.m3u` `.vgm` `.vgz` `.gym` | playlists, register logs (Mega Drive / Master System / arcade), Genesis logs |
| Nintendo | `.vgm` `.vgz` `.nsf` `.gbs` | register logs, NES/Famicom music, Game Boy music |
| Computer | `.m3u` `.mid` `.sid` | playlists, General MIDI (OPL synthesis), Commodore 64 music |

A playlist opened from any menu can list tracks of any supported format —
the player reads the `.m3u` and then opens each track by path.

VGM/VGZ appear in both console menus — packs for any platform may come in
these formats. Multi-song files (NSF/GBS/SID) switch subsongs with the D-pad.

## Simulated chips

| Chip | Found in | RTL |
|---|---|---|
| YM2612 (OPN2) | Sega Mega Drive / Genesis | [jt12](https://github.com/jotego/jt12) |
| SN76489 (PSG) | Master System, Game Gear, Mega Drive | [jt89](https://github.com/jotego/jt89) |
| YM2151 (OPM) | Sega/Konami arcade, Sharp X68000 | [jt51](https://github.com/jotego/jt51) |
| SegaPCM, MSM6258 | Sega arcade (OutRun etc.), X68000 | jtoutrun / custom |
| AY-3-8910 / YM2149 / Sunsoft 5B | MSX, ZX Spectrum, Famicom (5B) | [jt49](https://github.com/jotego/jt49) |
| NES APU + DMC, VRC6 | NES / Famicom | [NES_MiSTer](https://github.com/MiSTer-devel/NES_MiSTer) / custom |
| Game Boy APU (+ SM83 CPU) | Game Boy (GBS) | [VerilogBoy](https://github.com/zephray/VerilogBoy) |
| SID 6581/8580 (+ 6502 CPU) | Commodore 64 | [C64_MiSTer](https://github.com/MiSTer-devel/C64_MiSTer) (reSID) |
| YMF262 (OPL3, OPL2 subset) | General MIDI synthesis (GENMIDI patches from Freedoom) | [opl3_fpga](https://github.com/gtaylormb/opl3_fpga) |

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

## Controls

- **Left / Right** — previous/next track (or subsong within NSF/GBS/SID)
- **A** — pause, **B** — stop
- **R (hold)** — fast forward ×8
- **Select** — playlist browser

On start the core shows a title screen and waits for **A**, unless a file was
just picked through Load — then it plays straight away.

## Repository layout

- `core/` — the openFPGA core: `core_top.sv`, `chipbox.sv` (chip bus),
  Quartus project, `pkg/pocket/` — SD card files (json, assets)
- `core/lang/rust/examples/player/` — the Rust player/UI (RISC-V soft core)
- `rtl/chipbox/` — custom modules: GBS box (SM83+APU), VRC6, MSM6258
- `rtl/vendor/` — vendored chip RTL (see `VENDOR.md`, mostly GPL-3.0)
- `firmware/vgm-core/` — shared Rust parser library (format parsers,
  inflate, MD5 for HVSC song lengths)
- `sim/` — Verilator harnesses: `vgmplay` (VGM→WAV on a PC), `chipbox_tb`
  (self-tests of the whole path, including real NSF/GBS/SID files)
- `scripts/` — updater, artwork generation, demo tune generators,
  VGM→GYM converter
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
- [Arlet/verilog-6502](https://github.com/Arlet/verilog-6502) — 6502 for NSF/SID
- [gtaylormb/opl3_fpga](https://github.com/gtaylormb/opl3_fpga) — YMF262 (OPL3)
- [agg23/openfpga-litex](https://github.com/agg23/openfpga-litex) — base core:
  RISC-V SoC, APF bridge, openFPGA infrastructure
- [freedoom](https://github.com/freedoom/freedoom) — GENMIDI GM patches for
  MIDI synthesis; [dhepper/font8x8](https://github.com/dhepper/font8x8) — UI font

## License

Own code — [GPL-3.0](LICENSE) (dictated by the vendored jt51/jt12/NES/SID RTL).
Third-party module licenses are in their directories and in
`rtl/vendor/VENDOR.md`.
