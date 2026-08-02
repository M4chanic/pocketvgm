# Sound chip roadmap

Priorities are derived from the [vgmrips.net top packs](https://vgmrips.net/packs/top)
(top 200 packs surveyed). Counts are how many of those 200 packs use the chip.

Chips are split into **home** (consoles + home computers) and **arcade**. The
current focus is home systems: those chips are smaller (or reuse silicon we
already have), so they fit the FPGA alongside the RISC-V with healthy timing.
Arcade PCM chips are large and overflow the device — that work is kept but
deferred to a possible separate "arcade" build variant (see *FPGA area*).

VGM often pairs two chips per system, so a missing "partner" chip means the
game plays but is incomplete — those cases are called out.

## Implemented

### Home (consoles + home computers)

| Chip | ~packs | Used by | RTL |
|---|---|---|---|
| YM2612 / YM3438 (OPN2) | ~41 | Mega Drive / Genesis | jt12 |
| SN76489 (PSG, incl. T6W28) | ~38 | Mega Drive, Master System, Game Gear | jt89 |
| NES APU (2A03) + VRC6 | ~24 | NES / Famicom | NES_MiSTer + custom |
| AY-3-8910 / YM2149 | ~14 | MSX, ZX Spectrum, Famicom 5B | jt49 |
| SID 6581/8580 | — | Commodore 64 (SID files) | C64_MiSTer |
| Game Boy DMG APU | ~4 | Game Boy (GBS) | VerilogBoy |
| SCC / K051649 | ~6 | MSX (Konami) | IKASCC |
| YMF262 (OPL3) | ~1 | PC AdLib, General MIDI synth | opl3_fpga |
| YM2203 (OPN) | ~5 | PC-88/98, MSX | jt12 + jt49 |
| YM2608 (OPNA) | ~11 | NEC PC-8801/98 | jt12 + jt49 |
| HuC6280 PSG | ~7 | PC Engine / TG-16 | custom |
| Famicom Disk System | ~2 | FDS | custom |
| RF5C164 / RF5C68 | ~2 | Sega Mega CD / CD 32X | custom |

### Arcade

| Chip | ~packs | Used by | RTL |
|---|---|---|---|
| YM2151 (OPM) | ~53 | Sega/Konami/Namco/Taito arcade, **Sharp X68000** | jt51 |
| OKIM6258 (MSM6258 ADPCM) | ~15 | Sega arcade, **X68000** | custom |
| SegaPCM | ~7 | Sega Super Scaler arcade | jtoutrun |
| OKIM6295 | ~10 | Capcom / Toaplan arcade | jt6295 (**sim only**) |
| K053260 | ~5 | Konami arcade | jt053260 (**sim only**) |

YM2151 + OKIM6258 also cover the Sharp X68000 (a home computer), so X68000
music already plays through the arcade FM/ADPCM path.

## To implement

### Home (current focus)

1. **YM2413 (OPLL)** + **VRC7** — ~5 packs. MSX and NES; OPL with a fixed
   instrument ROM, cheap once OPL routing exists.

### Arcade (deferred — progress preserved)

- **OKIM6295, K053260** — fully integrated and self-tested, currently built for
  simulation only (gated behind `M4_SIM` in `chipbox.sv`). Ready to ship in an
  arcade build variant once area allows. Unlock Battle Garegga (#3),
  Street Fighter II, The Simpsons, TMNT: Turtles in Time.
- **C140 / C219** (Namco: Dragon Saber #1, Rolling Thunder 2) and **K054539**
  (Konami: X-Men, Xexex) — no open RTL exists; would need a custom core from
  the MAME model.
- QSound, YM2610 (Neo Geo), C352 (Namco), YMF278B — larger/harder.

The RF5C164 keeps its 64 KB of sample RAM in PSRAM, not in block RAM: 64 KB
would take 64 M10K blocks and only fourteen are free. The module drives an
address and waits for a byte; the chipbox arbiter serves it, as with OKIM6295
and K053260.

## FPGA area

The Cyclone V 5CEBA4F23C8 is nearly full with the RISC-V SoC plus the
implemented chips. Measurements (Quartus 21.1):

- All three arcade partners (SCC + OKIM6295 + K053260) overflow the device by
  ~1% even at maximum area optimization.
- Even two of them (SCC + OKIM6295) do not fit at speed-optimized settings
  (1962 vs 1848 LABs); they fit only with area optimization, which drops the
  system-clock slack to about −4 ns — worse than a normal build.

So the core ships **two bitstreams** in one package (APF allows up to 8, picked
by the user through `variants.json`):

| Variant | Chips | Covers |
|---|---|---|
| **Console** (default) | YM2612, SN76489, AY, NES APU + VRC6, SID, Game Boy, OPL3 (incl. VGM AdLib), SCC | Mega Drive, NES, MSX, C64, Game Boy, PC AdLib |
| **Arcade + X68k** | YM2151, SegaPCM, MSM6258, OKIM6295, K053260 | arcade boards and the Sharp X68000 |

Measured for the Console variant: 17 190 / 18 480 ALMs (93%) with −0.12 ns
worst-case slack — the best timing the project has had. Splitting the arcade
FM/PCM cluster out freed about 1 270 ALMs (YM2151 1 050, SegaPCM 116,
MSM6258 108), and the gating has to cover each chip's plumbing — clock
enables, ROM fetchers, sequencer states, mixer terms — not just the instance,
or the area is not actually freed.

The biggest remaining win is unrelated to sound: the RISC-V FPU costs 2 564
ALMs and the player only uses floating point when computing clock dividers.
