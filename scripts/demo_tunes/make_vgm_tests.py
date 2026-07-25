#!/usr/bin/env python3
"""Синтетические VGM для проверки чипов, у которых нет свободной музыки.

Файлы свои (CC0), поэтому их можно держать в репозитории и гонять в CI —
в отличие от копирайтных паков с vgmrips. Ноты выбраны так, чтобы высоту
можно было проверить численно.

Использование: make_vgm_tests.py <каталог>
"""
import struct
import sys
from pathlib import Path

TICK = 44100


def vgm(body: bytes, clocks: dict, loop_off: int | None = None) -> bytes:
    """Собрать VGM v1.61: заголовок 0x100 байт + тело."""
    hdr = bytearray(0x100)
    hdr[0:4] = b"Vgm "
    hdr[8:12] = (0x161).to_bytes(4, "little")
    # смещение данных считается от поля 0x34
    hdr[0x34:0x38] = (0x100 - 0x34).to_bytes(4, "little")
    total = sum_ticks(body)
    hdr[0x18:0x1C] = total.to_bytes(4, "little")
    if loop_off is not None:
        hdr[0x1C:0x20] = (0x100 + loop_off - 0x1C).to_bytes(4, "little")
        hdr[0x20:0x24] = total.to_bytes(4, "little")
    for off, val in clocks.items():
        hdr[off:off + 4] = val.to_bytes(4, "little")
    hdr[4:8] = (0x100 + len(body) - 4).to_bytes(4, "little")
    return bytes(hdr) + body


def sum_ticks(body: bytes) -> int:
    """Грубо: суммируем только команды ожидания 0x61."""
    t, i = 0, 0
    while i < len(body):
        c = body[i]
        if c == 0x61:
            t += int.from_bytes(body[i + 1:i + 3], "little")
            i += 3
        elif c == 0x66:
            break
        elif c == 0xD2:
            i += 4
        elif c in (0xBA, 0x5A, 0x52, 0x53, 0xA0, 0x54):
            i += 3
        elif c == 0x67:
            i += 7 + int.from_bytes(body[i + 3:i + 7], "little")
        else:
            i += 1
    return t


def wait(ms: int) -> bytes:
    n = int(TICK * ms / 1000)
    return bytes([0x61]) + n.to_bytes(2, "little")


# ---------------------------------------------------------------- SCC (MSX)
def scc_test() -> bytes:
    """AY + SCC: гамма на 1-м канале SCC, чтобы слышать волновую таблицу."""
    b = bytearray()
    scc = lambda port, reg, val: b.extend([0xD2, port, reg, val & 0xFF])

    # пила в волновой таблице канала 1 (32 знаковых отсчёта)
    for i in range(32):
        scc(0, i, (i * 8 - 128) & 0xFF)
    scc(2, 0, 0x0F)          # громкость канала 1
    scc(3, 0, 0x01)          # keyon канала 1

    # f = 1789772 / (32 * (freq + 1)); ноты A4..A5
    for hz in (440, 494, 523, 587, 659, 698, 784, 880):
        f = round(1789772 / (32 * hz)) - 1
        scc(1, 0, f & 0xFF)
        scc(1, 1, (f >> 8) & 0x0F)
        b += wait(300)
    scc(3, 0, 0x00)          # keyoff
    b += wait(100)
    b.append(0x66)
    # AY заявлен, чтобы файл выглядел как настоящий MSX-рип
    return vgm(bytes(b), {0x74: 1789772, 0x9C: 1789772})


# ------------------------------------------------------- K053260 (аркадный)
def k060_test() -> bytes:
    """K053260: PCM-синус из ROM на канале 0, пять нот разной высоты."""
    import math
    LEN = 128
    rom = bytes((round(100 * math.sin(2 * math.pi * i / LEN)) & 0xFF)
                for i in range(LEN))
    start = 0x100
    data = bytearray(b"\x00" * start) + bytearray(rom)

    b = bytearray()
    # блок данных ROM: [u32 полный размер][u32 смещение][данные]
    blk = struct.pack("<II", len(data), 0) + bytes(data)
    b += bytes([0x67, 0x66, 0x8E]) + len(blk).to_bytes(4, "little") + blk

    k = lambda reg, val: b.extend([0xBA, reg, val & 0xFF])
    k(0x2F, 0x02)            # mode[1]=1 — без него выход затухает в ноль
    k(0x28, 0x00)            # снять keyon (после сброса он 0xF)
    k(0x0A, LEN & 0xFF)      # длина
    k(0x0B, LEN >> 8)
    k(0x0C, start & 0xFF)    # адрес начала сэмпла
    k(0x0D, (start >> 8) & 0xFF)
    k(0x0E, (start >> 16) & 0x1F)
    k(0x0F, 0x60)            # громкость
    k(0x2A, 0x01)            # loop канала 0, ADPCM выключен
    k(0x2C, 0x04)            # пан по центру

    for pitch in (0xF00, 0xE00, 0xD00, 0xC00, 0xB00):
        k(0x08, pitch & 0xFF)
        k(0x09, (pitch >> 8) & 0x0F)
        k(0x28, 0x01)        # keyon канала 0 (фронт)
        b += wait(400)
    k(0x28, 0x00)
    b += wait(100)
    b.append(0x66)
    return vgm(bytes(b), {0xAC: 3579545})


if __name__ == "__main__":
    out = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    out.mkdir(parents=True, exist_ok=True)
    for name, data in (("scc_test.vgm", scc_test()),
                       ("k053260_test.vgm", k060_test())):
        (out / name).write_bytes(data)
        print(f"{out / name}: {len(data)} байт")
