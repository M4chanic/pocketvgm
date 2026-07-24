# Sound chip roadmap

Priorities are derived from the [vgmrips.net top packs](https://vgmrips.net/packs/top)
(top 200 packs surveyed). Counts are how many of those 200 packs use the chip;
they set the implementation order. VGM often pairs two chips per system, so a
missing "partner" chip means the game plays but is incomplete — those cases are
called out explicitly.

## Implemented

| Chip | ~packs (top 200) | Used by | RTL |
|---|---|---|---|
| YM2151 (OPM) | ~53 | Sega/Konami/Namco/Taito arcade, X68000 | jt51 |
| YM2612 / YM3438 (OPN2) | ~41 | Mega Drive / Genesis | jt12 |
| SN76489 (PSG, incl. T6W28) | ~38 | Mega Drive, Master System, arcade | jt89 |
| NES APU (2A03) + VRC6 | ~24 | NES / Famicom | NES_MiSTer + custom |
| OKIM6258 (MSM6258 ADPCM) | ~15 | Sega arcade, X68000 | custom |
| AY-3-8910 / YM2149 | ~14 | MSX, arcade, Famicom 5B | jt49 |
| SegaPCM | ~7 | Sega Super Scaler arcade | jtoutrun |
| YMF262 (OPL3) | ~1 (Doom) | AdLib / PC | opl3_fpga |
| Game Boy DMG APU | ~4 | Game Boy (GBS) | VerilogBoy |

The OPL3 core is already on the FPGA (used for MIDI synthesis), so routing VGM
OPL streams (YMF262 / YM3812 / YM3526) to it is mostly firmware work, not new
RTL — see the OPL family below.

## To implement, by priority

### Tier 1 — high count, or a missing partner that mutes popular packs

1. **OKIM6295 (MSM6295 ADPCM)** — ~10 packs. **Partner gap:** almost always
   paired with YM2151, which we already have. Without it, Battle Garegga (#3),
   Street Fighter II, Armed Police Batrider, Hyper Duel, Thunder Zone play FM
   but lose all drums/voice samples. Highest value: unlocks the whole
   YM2151+OKIM6295 arcade catalogue.
2. **YM2608 (OPNA)** — ~11 packs. Standalone (PC-8801/98, NEC): The Scheme,
   YU-NO, Grounseed, Snatcher, Brandish. OPNA = OPN2 + SSG + ADPCM + rhythm;
   partially reuses jt12/jt49 building blocks (jt10 family).
3. **OPL family for VGM: YM3812 (OPL2), YM3526 (OPL), YMF262 (OPL3)** —
   ~13 packs combined. The silicon already exists (OPL3). Unlocks PC AdLib
   (Doom, Monkey Island 2, Dune, Ys II) and arcade (Bubble Bobble). Mostly VGM
   command routing + OPL2/OPL3 mode select.

### Tier 2 — arcade PCM partners of chips we already have

4. **C140 / C219 (Namco)** — ~6 packs. **Partner gap:** paired with YM2151.
   Dragon Saber (#1 overall!), Rolling Thunder 2, Metal Hawk, Burning Force,
   Cyber Sled play FM but no PCM.
5. **K053260 (Konami PCM)** — ~5 packs. **Partner gap:** paired with YM2151.
   The Simpsons, TMNT: Turtles in Time, Detana!! TwinBee, Thunder Cross II.
6. **K051649 / SCC / SCC-I (Konami)** — ~6 packs. **Partner gap:** paired with
   AY-3-8910. MSX classics: Space Manbow (#5), Metal Gear 2 (#8), Salamander,
   Quarth, Snatcher — the SCC carries the melody, so these are badly hollowed
   out without it.
7. **K054539 (Konami PCM)** — ~4 packs. Paired with YM2151: X-Men, Xexex,
   Salamander 2, Polygonet.

### Tier 3 — standalone systems (need both chip + often a CPU/wavetable)

8. **YM2610 / YM2610B (OPNB)** — ~8 packs. Neo Geo: Metal Slug, KOF '99,
   Darius II, Viewpoint, Metal Black. OPNB = OPN2 + SSG + 2× ADPCM.
9. **HuC6280** — ~7 packs. PC Engine / TG-16: Devil Crash, Soldier Blade,
   Aldynes, Bomberman '94. Wavetable, CPU-integrated.
10. **C352 (Namco)** — ~8 packs. System 22/12/ND-1: Ridge Racer 1/2, Xevious
    3D/G, Fighting Layer. Large PCM chip.
11. **YM2203 (OPN)** — ~5 packs. PC-88/98, arcade: Sorcerian, EVE, Xenon.
    3 FM + SSG; close to jt12/jt49 family.
12. **YM2413 (OPLL)** — ~4 packs, plus **VRC7** (~1, Lagrange Point) which is
    an OPLL variant. **Partner gap:** paired with AY on MSX (Final Fantasy MSX,
    Illusion City, Xak, Dragon Slayer). OPL2 with fixed instrument ROM — cheap
    once OPL is in.

### Tier 4 — lower count / harder

- **QSound** — ~9 packs (CPS2: Street Fighter Alpha, MvC). DSP-based, complex.
- **RF5C68 / RF5C164 (Ricoh PCM)** — ~3 packs (Mega CD, FM Towns).
- **YMF278B (OPL4)** / **YMF271 (OPX)** / **YMW258 (MultiPCM)** — a few packs
  each; large wavetable/PCM chips.
- **uPD7759, GA20, PWM (32X), VSU-VUE (Virtual Boy)** — 1 pack each.

## Summary

The six most common chips (YM2151, YM2612, SN76489, NES APU, OKIM6258, AY) are
already implemented, covering the bulk of the top packs. The biggest remaining
wins are the **arcade ADPCM/PCM partners** — OKIM6295, C140, K053260, SCC —
because each one completes many packs whose FM half already plays here.

## Note on FPGA area

SCC, OKIM6295 and K053260 together overflow the Cyclone V by ~1% at maximum
area optimization. To fit, **K053260 runs with 2 channels instead of 4**
(`NUMCH=2` on the `jt053260` instance in `chipbox.sv`). The three chips target
mutually exclusive platforms (MSX / Capcom / Konami), so they are never needed
at once; the 2-channel limit only affects busy K053260 tracks. It can be raised
back to 4 once area is freed elsewhere.
