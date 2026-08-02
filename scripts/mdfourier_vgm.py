#!/usr/bin/env python3
"""Генератор VGM с тоновым сигналом MDFourier для Mega Drive.

Зачем. MDFourier (Artemio Urbina, http://junkerhq.net/MDFourier/) меряет
звуковой тракт по известному стимулу: 96 тонов FM, 40 тонов PSG, рампа из
400 ступеней и 16 вариантов шума, с синхроимпульсами по краям. К нему
приложена база записей с НАСТОЯЩИХ консолей — десяток ревизий Mega Drive.
Сравнив свой рендер с такой записью, получаешь кривую расхождения по
каждой частоте, а не по девяти октавным полосам, как даёт band_curve.py.

Загвоздка была в том, что генератор сигнала — это ROM для Mega Drive, а
наше ядро играет VGM. Но генератор (240psuite/Genesis/240p/mdfourier.c,
GPL-2) не делает ничего, кроме записей в регистры YM2612 и PSG с паузой в
кадр между ними, а это ровно то, из чего состоит VGM. Здесь та же
последовательность собрана напрямую из исходника.

Порядок регистров, номера, патчи и покадровый тайминг воспроизведены
буквально — вплоть до того, что в блоке шума огибающая считается как
frame/(framelen/15) и на последних кадрах переполняется в младшие четыре
бита. Это не ошибка, так делает железо, и эталонная запись такая же.

Использование:
    python3 scripts/mdfourier_vgm.py выход.vgm
    sim/chipbox_tb/chipbox_tb выход.vgm -o наш.wav -t 60
    mdfourier -P профили/mdfblocksGEN.mfn -r железо.flac -c наш.wav -C
"""

import struct
import sys

# Тактовые как у настоящей приставки NTSC
YM_CLOCK = 7670454
PSG_CLOCK = 3579545

# Длина кадра в отсчётах шкалы VGM (44100 Гц). У NTSC Mega Drive кадр
# 16.6884 мс — так записано и в профиле MDFourier, — то есть 59.9218 Гц
# и 736.03 отсчёта. Стандартная команда 0x62 дала бы 735 и ровно 60 Гц,
# и за три с половиной тысячи кадров набежал бы кадр расхождения.
FRAME_SAMPLES = 736

FRAMELEN = 20          # кадров на один тон, как вызывается в ROM
PULSE_TRAIN_FREQ = 8820

# Таблица высот из mdfourier.c. Это прямо 11-битный F-number YM2612,
# а не герцы: старшие три бита уходят в регистр 0xA4, младшие восемь в 0xA0.
PITCHES = [277, 293, 311, 329, 349, 369, 391, 415, 439, 465, 493, 522]

STEREO_RIGHT = 0x40
STEREO_LEFT = 0x80
STEREO_BOTH = 0xC0

PSG_ENV_MIN = 15       # тише некуда
PSG_ENV_MAX = 0        # громче некуда
NOISE_PERIODIC, NOISE_WHITE = 0, 1
NF_CLOCK2, NF_CLOCK4, NF_CLOCK8, NF_TONE3 = 0, 1, 2, 3


class Vgm:
    """Накопитель команд VGM со счётчиком отсчётов."""

    def __init__(self):
        self.buf = bytearray()
        self.samples = 0

    # --- YM2612 -------------------------------------------------------
    def ym(self, part, reg, val):
        self.buf += bytes((0x52 + (1 if part else 0), reg & 0xFF, val & 0xFF))

    # --- SN76489 (в исходнике он зовётся PSG) --------------------------
    def psg(self, data):
        self.buf += bytes((0x50, data & 0xFF))

    def wait_frame(self):
        self.buf += b"\x61" + struct.pack("<H", FRAME_SAMPLES)
        self.samples += FRAME_SAMPLES

    def end(self):
        self.buf += b"\x66"


# ----------------------------------------------------------------------
# PSG: арифметика один в один из SGDK (src/psg.c)

def psg_set_tone(v, channel, value):
    v.psg(0x80 | ((channel & 3) << 5) | (value & 0xF))
    v.psg((value >> 4) & 0x3F)


def psg_set_frequency(v, channel, freq):
    data = PSG_CLOCK // (freq * 32) if freq else 0
    psg_set_tone(v, channel, data)


def psg_set_envelope(v, channel, value):
    v.psg(0x90 | ((channel & 3) << 5) | (value & 0xF))


def psg_set_noise(v, ntype, freq):
    v.psg(0xE0 | ((ntype & 1) << 2) | (freq & 3))


def psg_stop(v):
    for ch in range(4):
        psg_set_envelope(v, ch, PSG_ENV_MIN)


def psg_reset(v):
    for ch in range(4):
        psg_set_tone(v, ch, 0)
        psg_set_envelope(v, ch, PSG_ENV_MIN)


# ----------------------------------------------------------------------
# YM2612

def _part_ch(channel):
    """Канал 0-5 -> (часть, канал внутри части)."""
    return (0, channel) if channel < 3 else (1, channel - 3)


def _key_code(part, channel):
    """Код канала для регистра 0x28."""
    return channel + 4 if part else channel


def ym_key_off(v, channel):
    part, ch = _part_ch(channel)
    v.ym(0, 0x28, _key_code(part, ch))


def ym_key_off_all(v):
    for c in range(6):
        ym_key_off(v, c)


def ym_instrument_load(v, channel):
    """Патч для каналов 0-2."""
    part, ch = _part_ch(channel)
    v.ym(part, 0x22, 0x00)
    v.ym(part, 0x27, 0x00)
    for base, val in ((0x30, 0x06), (0x34, 0x06), (0x38, 0x06), (0x3C, 0x06)):
        v.ym(part, base + ch, val)
    for base, val in ((0x40, 0x32), (0x44, 0x21), (0x48, 0x73), (0x4C, 0x00)):
        v.ym(part, base + ch, val)
    for base in (0x50, 0x54, 0x58, 0x5C):
        v.ym(part, base + ch, 0x0F)
    for base in (0x60, 0x64, 0x68, 0x6C):
        v.ym(part, base + ch, 0x0F)
    for base in (0x70, 0x74, 0x78, 0x7C):
        v.ym(part, base + ch, 0x0A)
    for base in (0x80, 0x84, 0x88, 0x8C):
        v.ym(part, base + ch, 0x08)
    for base in (0x90, 0x94, 0x98, 0x9C):
        v.ym(part, base + ch, 0x00)
    v.ym(part, 0xB0 + ch, 0x01)
    v.ym(part, 0xB4 + ch, STEREO_BOTH)
    v.ym(0, 0x28, _key_code(part, ch))


def ym_grand_piano_load(v, channel):
    """Патч для каналов 3-5."""
    part, ch = _part_ch(channel)
    v.ym(part, 0x22, 0x00)
    v.ym(part, 0x27, 0x00)
    for base, val in ((0x30, 0x71), (0x34, 0x0D), (0x38, 0x33), (0x3C, 0x01)):
        v.ym(part, base + ch, val)
    for base, val in ((0x40, 0x23), (0x44, 0x2D), (0x48, 0x26), (0x4C, 0x00)):
        v.ym(part, base + ch, val)
    for base, val in ((0x50, 0x5F), (0x54, 0x99), (0x58, 0x5F), (0x5C, 0x94)):
        v.ym(part, base + ch, val)
    for base, val in ((0x60, 0x05), (0x64, 0x05), (0x68, 0x05), (0x6C, 0x07)):
        v.ym(part, base + ch, val)
    for base in (0x70, 0x74, 0x78, 0x7C):
        v.ym(part, base + ch, 0x02)
    for base, val in ((0x80, 0x11), (0x84, 0x11), (0x88, 0x11), (0x8C, 0xA6)):
        v.ym(part, base + ch, val)
    for base in (0x90, 0x94, 0x98, 0x9C):
        v.ym(part, base + ch, 0x00)
    v.ym(part, 0xB0 + ch, 0x32)
    v.ym(part, 0xB4 + ch, STEREO_BOTH)
    v.ym(0, 0x28, _key_code(part, ch))


def ym_play(v, channel, note, octave, pan):
    part, ch = _part_ch(channel)
    v.ym(part, 0xB4 + ch, pan)
    v.ym(0, 0x28, _key_code(part, ch))          # key off
    v.ym(part, 0xA4 + ch, octave + (PITCHES[note] >> 8))
    v.ym(part, 0xA0 + ch, PITCHES[note] & 0xFF)
    v.ym(0, 0x28, 0xF0 + _key_code(part, ch))   # key on, все операторы


def ym_init(v):
    # Сброс: гасим LFO, таймеры, ЦАП и все ноты. В SGDK YM2612_reset ещё
    # выкручивает уровни, но следом всё равно грузятся патчи, которые
    # задают их сами.
    for part in (0, 1):
        v.ym(part, 0x22, 0x00)
        v.ym(part, 0x27, 0x00)
    v.ym(0, 0x2B, 0x00)                          # ЦАП выключен
    ym_key_off_all(v)
    for c in range(3):
        ym_instrument_load(v, c)
    for c in range(3, 6):
        ym_grand_piano_load(v, c)


# ----------------------------------------------------------------------
# Блоки последовательности

def execute_pulse_train(v):
    """Синхроимпульс: по нему MDFourier находит границы блоков."""
    psg_set_frequency(v, 0, PULSE_TRAIN_FREQ)
    for _ in range(10):
        psg_set_envelope(v, 0, PSG_ENV_MAX)
        v.wait_frame()
        psg_set_envelope(v, 0, PSG_ENV_MIN)
        v.wait_frame()


def execute_silence(v):
    for _ in range(20):
        v.wait_frame()


def execute_fm(v, framelen):
    for octave in range(0, 57, 8):
        chann = 0
        for note in range(12):
            ym_play(v, chann, note, octave, STEREO_LEFT)
            ym_play(v, chann + 3, note, octave, STEREO_RIGHT)
            for frame in range(framelen):
                if frame == framelen - framelen // 5:
                    ym_key_off(v, chann)
                    ym_key_off(v, chann + 3)
                v.wait_frame()
            chann = 0 if chann >= 2 else chann + 1
    ym_key_off_all(v)


def execute_psg(v, framelen):
    for freq in range(500, 20001, 500):
        psg_set_frequency(v, 0, freq)
        psg_set_envelope(v, 0, PSG_ENV_MAX)
        for frame in range(framelen):
            if frame == framelen - 1:
                psg_stop(v)
            v.wait_frame()
    psg_stop(v)


def execute_psg_ramp(v):
    for freq in range(50, 20001, 50):
        psg_set_frequency(v, 0, freq)
        psg_set_envelope(v, 0, PSG_ENV_MAX)
        v.wait_frame()
    psg_stop(v)


def execute_noise(v, framelen):
    types = (NOISE_WHITE, NOISE_PERIODIC)
    clocks = (NF_CLOCK2, NF_CLOCK4, NF_CLOCK8, NF_TONE3)
    for envel in range(2):
        for ntype in types:
            for clk in clocks:
                if clk == NF_TONE3:
                    psg_set_frequency(v, 2, 4000)
                psg_set_noise(v, ntype, clk)
                psg_set_envelope(v, 3, PSG_ENV_MAX)
                for frame in range(framelen):
                    if envel:
                        # framelen/15 в целых даёт 1, поэтому уровень равен
                        # номеру кадра и на кадрах 16-19 заворачивается в
                        # младшие четыре бита. Так в исходнике, так и на
                        # железе — повторяем буквально.
                        psg_set_envelope(v, 3, frame // (framelen // 0x0F))
                    if frame == framelen - 1:
                        psg_stop(v)
                    v.wait_frame()
    psg_stop(v)


def build():
    v = Vgm()
    ym_init(v)
    psg_reset(v)
    v.wait_frame()

    execute_pulse_train(v)
    execute_silence(v)

    execute_fm(v, FRAMELEN)
    execute_psg(v, FRAMELEN)
    execute_psg_ramp(v)
    execute_noise(v, FRAMELEN)

    execute_silence(v)
    execute_pulse_train(v)

    # Хвост тишины: без него последний синхроимпульс упирается в конец
    # файла, и его край нечем отбить.
    psg_stop(v)
    ym_key_off_all(v)
    for _ in range(10):
        v.wait_frame()
    v.end()
    return v


def write_vgm(path, v):
    data_off = 0x100
    hdr = bytearray(data_off)
    hdr[0x00:0x04] = b"Vgm "
    struct.pack_into("<I", hdr, 0x04, data_off + len(v.buf) - 4)   # EOF от 0x04
    struct.pack_into("<I", hdr, 0x08, 0x00000161)                  # версия 1.61
    struct.pack_into("<I", hdr, 0x0C, PSG_CLOCK)
    struct.pack_into("<I", hdr, 0x18, v.samples)
    struct.pack_into("<I", hdr, 0x24, 60)                          # частота кадров
    struct.pack_into("<H", hdr, 0x28, 0x0009)                      # обратная связь шума
    hdr[0x2A] = 16                                                 # разрядность регистра сдвига
    hdr[0x2B] = 0
    struct.pack_into("<I", hdr, 0x2C, YM_CLOCK)
    struct.pack_into("<I", hdr, 0x34, data_off - 0x34)
    with open(path, "wb") as f:
        f.write(bytes(hdr) + bytes(v.buf))


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 1
    v = build()
    write_vgm(sys.argv[1], v)
    frames = v.samples // FRAME_SAMPLES
    print("записан %s: %d кадров, %.1f с, %d байт команд"
          % (sys.argv[1], frames, v.samples / 44100.0, len(v.buf)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
