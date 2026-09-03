# Мини-ассемблер 6502 для демо-мелодий (только нужные опкоды).
# Двухпроходный: первый проход собирает адреса меток, второй — байты.

OPS = {
    ('LDA', 'imm'): 0xA9, ('LDA', 'abs'): 0xAD, ('LDA', 'indy'): 0xB1,
    ('STA', 'abs'): 0x8D, ('STA', 'zp'): 0x85,
    ('LDY', 'imm'): 0xA0, ('LDX', 'imm'): 0xA2, ('DEX', '-'): 0xCA,
    ('INY', '-'): 0xC8,
    ('DEC', 'zp'): 0xC6,
    ('INC', 'zp'): 0xE6,
    ('CMP', 'imm'): 0xC9,
    ('CLC', '-'): 0x18, ('ADC', 'imm'): 0x69, ('ADC', 'zp'): 0x65,
    ('LDA', 'zp'): 0xA5,
    ('BNE', 'rel'): 0xD0, ('BEQ', 'rel'): 0xF0, ('BMI', 'rel'): 0x30,
    ('JMP', 'abs'): 0x4C, ('JSR', 'abs'): 0x20,
    ('RTS', '-'): 0x60, ('SEI', '-'): 0x78, ('RTI', '-'): 0x40,
}


class Asm:
    def __init__(self, org):
        self.org = org
        self.items = []   # (mnemonic, mode, arg) | ('label', name) | ('bytes', data)
        self.labels = {}

    def label(self, name):
        self.items.append(('label', name, None))

    def op(self, mn, mode='-', arg=None):
        self.items.append((mn, mode, arg))

    def data(self, b):
        self.items.append(('bytes', None, bytes(b)))

    def _size(self, mn, mode):
        if mode in ('-',):
            return 1
        if mode in ('imm', 'zp', 'rel', 'indy'):
            return 2
        return 3  # abs

    def _resolve(self, arg):
        if isinstance(arg, str):
            if arg.startswith('<'):
                return self.labels[arg[1:]] & 0xFF
            if arg.startswith('>'):
                return self.labels[arg[1:]] >> 8
            return self.labels[arg]
        return arg

    def assemble(self):
        # проход 1: адреса меток
        pc = self.org
        for mn, mode, arg in self.items:
            if mn == 'label':
                self.labels[mode] = pc
            elif mn == 'bytes':
                pc += len(arg)
            else:
                pc += self._size(mn, mode)
        # проход 2: байты
        out = bytearray()
        pc = self.org
        for mn, mode, arg in self.items:
            if mn == 'label':
                continue
            if mn == 'bytes':
                out += arg
                pc += len(arg)
                continue
            opc = OPS[(mn, mode)]
            size = self._size(mn, mode)
            out.append(opc)
            if mode == 'rel':
                tgt = self._resolve(arg)
                off = tgt - (pc + 2)
                assert -128 <= off <= 127, f'branch {mn} {arg} слишком далеко ({off})'
                out.append(off & 0xFF)
            elif size == 2:
                v = self._resolve(arg)
                assert 0 <= v <= 0xFF, f'{mn} {mode} {arg}: {v:#x} не байт'
                out.append(v)
            elif size == 3:
                v = self._resolve(arg)
                out.append(v & 0xFF)
                out.append(v >> 8)
            pc += size
        return bytes(out)


def note_hz(name):
    """'A4' / 'C#5' / 'Eb3' -> частота в Гц (A4 = 440)."""
    semis = {'C': 0, 'D': 2, 'E': 4, 'F': 5, 'G': 7, 'A': 9, 'B': 11}
    n = semis[name[0]]
    rest = name[1:]
    if rest[0] == '#':
        n += 1
        rest = rest[1:]
    elif rest[0] == 'b':
        n -= 1
        rest = rest[1:]
    octave = int(rest)
    midi = 12 * (octave + 1) + n
    return 440.0 * 2 ** ((midi - 69) / 12)
