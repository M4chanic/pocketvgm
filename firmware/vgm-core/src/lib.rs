//! Парсер и секвенсор формата VGM (1.50–1.71) для m4pocket.
//!
//! no_std + alloc: работает и в фирмвари RISC-V, и в тестах на хосте.
//! Парсер ничего не знает о железе: он выдаёт поток событий
//! ([`Event`]), а маршрутизация записей в конкретные чипы — дело плеера.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod md5;

/// zlib-поток (GYMX-сжатие) -> байты; None при ошибке
pub fn decompress_zlib(data: &[u8]) -> Option<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib(data).ok()
}

use alloc::vec::Vec;

pub const VGM_MAGIC: &[u8; 4] = b"Vgm ";
pub const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Тактовая частота тиков VGM: все ожидания измеряются в 1/44100 c.
pub const TICK_RATE: u32 = 44100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    Sn76489,
    Ym2413,
    Ym2612,
    Ym2151,
    SegaPcm,
    Ay8910,
    NesApu,
    GbDmg,
    Okim6258,
    Okim6295,
    K051649,
    K053260,
    /// OPN-семейство (YM2203/YM2608). Отдельного RTL под них нет, но
    /// FM-часть регистрово совместима с нашим YM2612, а SSG — это ровно
    /// наш jt49. `port` = банк регистров (0 или 1; у YM2203 всегда 0).
    Opn,
    /// HuC6280 PSG (PC Engine / TurboGrafx-16)
    HuC6280,
    /// OPL-семейство (YM3812/YM3526/YMF262) — играется на нашем OPL3.
    /// `port` = банк регистров (0 или 1; у OPL2 всегда 0).
    Opl,
    Unknown(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Запись в регистр чипа. `port` различает банки регистров
    /// (у YM2612 два порта; у остальных 0).
    Write { chip: Chip, port: u8, addr: u8, data: u8 },
    /// Запись в память SegaPCM (смещение 16 бит).
    SegaPcmWrite { offset: u16, data: u8 },
    /// Подождать `ticks` тиков по 1/44100 с.
    Wait { ticks: u16 },
    /// Блок данных (ROM/RAM-образ для PCM-чипов): тип по спецификации VGM
    /// и границы данных внутри исходного буфера.
    DataBlock { kind: u8, start: usize, len: usize },
    /// Команды DAC-стримов 0x90–0x95 (пока прозрачно передаются плееру).
    DacStream { cmd: u8, start: usize, len: usize },
    /// YM2612 DAC-байт из data-банка 0x00 (команды 0x80-0x8F): записать
    /// байт банка по offset в регистр 0x2A и подождать ticks тиков.
    Ym2612Dac { ticks: u8, offset: u32 },
    /// Маска стерео Game Gear: команда 0x4F (и 0x3F для второго чипа).
    /// По биту на канал и сторону, порт 0x06.
    ///
    /// Это НЕ запись в регистр PSG, а раньше было склеено с 0x50, то есть
    /// маска уходила в чип как байт данных. Обычное значение 0xFF (всё
    /// звучит в оба уха) для SN76489 — байт с битом 7, то есть latch:
    /// канал 3, тип «громкость», данные 0xF. Иначе говоря, каждая такая
    /// запись ГЛУШИЛА канал шума. Файлы Game Gear пишут маску часто.
    GgStereo { chip2: bool, mask: u8 },
    /// Запись, адресованная ВТОРОМУ экземпляру чипа.
    ///
    /// Второго экземпляра в железе нет, играть эту запись нечем. Но и
    /// терять её молча нельзя: команды 0xA1-0xAF (зеркало 0x51-0x5F для
    /// второго чипа) раньше проваливались в ветку «пропустить и забыть»,
    /// и dual-chip файл лишался половины музыки без всякого сообщения. У
    /// чипов с коротким регистровым полем второй экземпляр выбирается
    /// битом 7 байта регистра, и этот бит доезжал до чипа: записи второго
    /// экземпляра садились на регистры ПЕРВОГО и дрались с ними.
    SecondChip { cmd: u8 },
    /// Конец звуковых данных.
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadMagic,
    TooShort,
    BadOffset,
    UnknownCommand { cmd: u8, pos: usize },
    Gzip,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Clocks {
    pub sn76489: u32,
    pub ym2413: u32,
    pub ym2612: u32,
    pub ym2203: u32,
    pub ym2608: u32,
    pub ym2151: u32,
    pub sega_pcm: u32,
    pub sega_pcm_iface: u32,
    pub ay8910: u32,
    pub nes_apu: u32,
    pub gb_dmg: u32,
    pub okim6258: u32,
    pub okim6258_flags: u8,
    pub ay_flags: u8,
    pub k051649: u32,
    pub okim6295: u32,
    pub k053260: u32,
    /// HuC6280 (PC Engine) — волновая таблица на 6 каналов
    pub huc6280: u32,
    /// OPL-семейство: YM3812 (OPL2), YM3526 (OPL), YMF262 (OPL3)
    pub ym3812: u32,
    pub ym3526: u32,
    pub ymf262: u32,
    /// Чипы, которые мы не играем, но обязаны опознать: молчать о них
    /// хуже, чем честно сказать, что часть звука не воспроизводится.
    /// Адреса полей сняты с настоящих файлов, а не со спецификации.
    pub pwm: u32,
    pub upd7759: u32,
    pub wonderswan: u32,
    /// PCM Sega CD (RF5C164) и его аркадный родич RF5C68: на дисках Mega
    /// CD почти вся музыка идёт через них, без них файл звучит тихо или
    /// молчит вовсе
    pub rf5c164: u32,
    pub rf5c68: u32,
    /// Дисковая приставка Famicom: старший бит клока NES APU
    pub fds: bool,
    /// Второй SN76489 (бит 30) и вариант T6W28 (бит 31). На Neo Geo
    /// Pocket выставлены оба: это один чип с двумя сторонами стерео.
    pub sn_dual: bool,
    pub sn_t6w28: bool,
    /// Файл объявляет ВТОРОЙ экземпляр какого-нибудь чипа: бит 30 в поле
    /// тактовой. Второго экземпляра у нас нет, его записи отбрасываются —
    /// но сказать об этом надо, иначе половина музыки пропадает молча.
    /// Случай T6W28 сюда не относится: там у SN76489 выставлены биты 30 и
    /// 31 сразу, и это один чип с двумя сторонами стерео.
    pub second_chip: bool,
}

/// Одна запись таблицы громкостей из extra header.
///
/// `chip` — номер чипа по спецификации VGM; бит 7 означает ПАРНУЮ часть
/// составного чипа (у YM2203 и YM2608 это SSG, а не FM). `raw` — сырое
/// поле: бит 15 отличает относительную громкость от абсолютной.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChipVolume {
    pub chip: u8,
    pub instance: u8,
    pub raw: u16,
}

pub const MAX_CHIP_VOLUMES: usize = 8;

/// Разобранный заголовок VGM. Владение данными остаётся у вызывающего.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub version: u32,
    pub total_ticks: u32,
    pub loop_offset: Option<usize>,
    pub loop_ticks: u32,
    pub data_offset: usize,
    pub gd3_offset: Option<usize>,
    pub clocks: Clocks,
    /// Общий модификатор громкости (поле 0x7C), уже со снятым знаком.
    /// Множитель = 2^(значение/32). Ноль означает «без изменений».
    pub volume_modifier: i16,
    /// Громкости по чипам из extra header (VGM 1.70+). Автор рипа
    /// выравнивает баланс именно здесь, и у составных чипов это
    /// единственное место, где сказано, насколько SSG громче или тише FM.
    pub chip_volumes: [ChipVolume; MAX_CHIP_VOLUMES],
    pub chip_volume_count: u8,
}

fn rd32(d: &[u8], off: usize) -> Result<u32, Error> {
    let b = d.get(off..off + 4).ok_or(Error::TooShort)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Объявлен ли второй экземпляр чипа: бит 30 поля тактовой.
fn dual_bit(d: &[u8], off: usize, hdr_end: usize) -> bool {
    off + 4 <= hdr_end && rd32(d, off).unwrap_or(0) & 0x4000_0000 != 0
}

/// Читает 32-битное поле часов, если оно внутри заголовка (иначе 0).
fn clock_field(d: &[u8], off: usize, hdr_end: usize) -> u32 {
    if off + 4 <= hdr_end {
        rd32(d, off).unwrap_or(0) & 0x3FFF_FFFF
    } else {
        0
    }
}

impl Header {
    pub fn parse(d: &[u8]) -> Result<Header, Error> {
        if d.len() < 0x40 {
            return Err(Error::TooShort);
        }
        if &d[0..4] != VGM_MAGIC {
            return Err(Error::BadMagic);
        }
        let version = rd32(d, 0x08)?;
        let data_offset = if version >= 0x150 {
            0x34 + rd32(d, 0x34)? as usize
        } else {
            0x40
        };
        if data_offset >= d.len() {
            return Err(Error::BadOffset);
        }
        // Поля часов действительны только до начала данных.
        let hdr_end = data_offset.min(0x100);

        let loop_off_raw = rd32(d, 0x1C)?;
        let loop_offset = if loop_off_raw != 0 {
            let o = 0x1C + loop_off_raw as usize;
            if o >= d.len() {
                return Err(Error::BadOffset);
            }
            Some(o)
        } else {
            None
        };
        let gd3_raw = rd32(d, 0x14)?;
        let gd3_offset = if gd3_raw != 0 { Some(0x14 + gd3_raw as usize) } else { None };

        let clocks = Clocks {
            sn76489: clock_field(d, 0x0C, hdr_end),
            ym2413: clock_field(d, 0x10, hdr_end),
            ym2612: clock_field(d, 0x2C, hdr_end),
            ym2203: clock_field(d, 0x44, hdr_end),
            ym2608: clock_field(d, 0x48, hdr_end),
            huc6280: clock_field(d, 0xA4, hdr_end),
            ym2151: clock_field(d, 0x30, hdr_end),
            sega_pcm: clock_field(d, 0x38, hdr_end),
            sega_pcm_iface: if version >= 0x151 { clock_field(d, 0x3C, hdr_end) } else { 0 },
            ay8910: clock_field(d, 0x74, hdr_end),
            ay_flags: if hdr_end > 0x79 { d[0x79] } else { 0 },
            gb_dmg: clock_field(d, 0x80, hdr_end),
            nes_apu: clock_field(d, 0x84, hdr_end),
            okim6258: clock_field(d, 0x90, hdr_end),
            okim6258_flags: if hdr_end > 0x94 { d[0x94] } else { 0 },
            k051649: clock_field(d, 0x9C, hdr_end),
            okim6295: clock_field(d, 0x98, hdr_end),
            k053260: clock_field(d, 0xAC, hdr_end),
            ym3812: clock_field(d, 0x50, hdr_end),
            ym3526: clock_field(d, 0x54, hdr_end),
            ymf262: clock_field(d, 0x5C, hdr_end),
            // Не играем, но опознаём: молчать о таком звуке хуже, чем
            // честно показать, что часть его не воспроизводится.
            pwm: clock_field(d, 0x70, hdr_end),
            upd7759: clock_field(d, 0x8C, hdr_end),
            wonderswan: clock_field(d, 0xC0, hdr_end),
            rf5c164: clock_field(d, 0x6C, hdr_end),
            rf5c68: clock_field(d, 0x40, hdr_end),
            // Флаг дисковой приставки — старший бит поля NES APU, и
            // читать его надо до маскирования: clock_field снимает
            // старшие два бита, потому что там живут признаки чипа.
            fds: hdr_end >= 0x88 && rd32(d, 0x84).unwrap_or(0) & 0x8000_0000 != 0,
            sn_dual: rd32(d, 0x0C).unwrap_or(0) & 0x4000_0000 != 0,
            sn_t6w28: rd32(d, 0x0C).unwrap_or(0) & 0x8000_0000 != 0,
            // Смещения — те же поля тактовых, что читаем выше. У SN76489
            // (0x0C) бит 30 сам по себе не считается: вместе с битом 31
            // он означает T6W28, то есть один чип, а не два.
            second_chip: [
                0x10, 0x2C, 0x30, 0x38, 0x44, 0x48, 0x50, 0x54, 0x5C, 0x74, 0x80, 0x84, 0x90,
                0x98, 0x9C, 0xA4, 0xAC,
            ]
            .iter()
            .any(|&o| dual_bit(d, o, hdr_end))
                || (dual_bit(d, 0x0C, hdr_end)
                    && rd32(d, 0x0C).unwrap_or(0) & 0x8000_0000 == 0),
        };

        // Модификатор громкости: множитель 2^(v/32). Разворот знака взят
        // с libvgm (vgmplayer.cpp), вместе с его особым случаем 0xC1 —
        // тот даёт -0x40, а не -0x3F, как вышло бы по общему правилу.
        let volume_modifier = if hdr_end > 0x7C {
            let v = d[0x7C];
            if v <= 0xC0 {
                v as i16
            } else if v == 0xC1 {
                -0x40
            } else {
                v as i16 - 0x100
            }
        } else {
            0
        };

        let (chip_volumes, chip_volume_count) = parse_chip_volumes(d, hdr_end);

        Ok(Header {
            version,
            total_ticks: rd32(d, 0x18)?,
            loop_offset,
            loop_ticks: rd32(d, 0x20)?,
            data_offset,
            gd3_offset,
            clocks,
            volume_modifier,
            chip_volumes,
            chip_volume_count,
        })
    }
}

/// Базовые громкости чипов, снятые с libvgm (`_CHIP_VOLUME` в
/// player/vgmplayer.cpp). Нужны только для АБСОЛЮТНЫХ записей: там
/// сказано «поставь столько-то» в шкале libvgm, и превратить это в
/// поправку к нашему гейну можно лишь через отношение к тому, что стояло
/// бы по умолчанию. Для относительных записей таблица не нужна вовсе.
const CHIP_BASE_VOL: [u16; 32] = [
    0x80, 0x200, 0x100, 0x100, 0x180, 0xB0, 0x100, 0x80, // SN76489..YM2608
    0x80, 0x100, 0x100, 0x100, 0x100, 0x100, 0x100, 0x98, // YM2610..YMZ280B
    0x80, 0xE0, 0x100, 0xC0, 0x100, 0x40, 0x11E, 0x1C0, // RF5C164..OKIM6258
    0x100, 0xA0, 0x100, 0x100, 0x100, 0x100, 0x100, 0x100, // OKIM6295..QSound
];

/// 2^(k/32) в 1/256 долях
const POW2_32: [u32; 32] = [
    256, 262, 267, 273, 279, 285, 292, 298, 304, 311, 318, 325, 332, 339, 347, 354, 362, 370,
    378, 386, 395, 403, 412, 421, 431, 440, 450, 459, 470, 480, 490, 501,
];

impl Header {
    /// Общий множитель громкости файла, в 1/256 долях (256 = без правки).
    ///
    /// Целочисленно: степень двойки разложена на целую часть (сдвиг) и
    /// дробную (таблица на 32 значения). Вещественной арифметики в
    /// фирмвари нет — FPU из софткора вырезан.
    pub fn volume_scale(&self) -> u32 {
        let v = self.volume_modifier;
        let i = v >> 5; // арифметический сдвиг даёт целую часть вниз
        let frac = POW2_32[(v & 31) as usize];
        if i >= 0 {
            frac << (i.min(8) as u32)
        } else {
            frac >> ((-i).min(8) as u32)
        }
    }

    /// Во сколько раз файл просит изменить громкость чипа, в 1/256 долях.
    ///
    /// `chip` — номер по спецификации VGM, бит 7 для парной части
    /// (у YM2203/YM2608 это SSG). 256 означает «ничего не менять».
    pub fn chip_scale(&self, chip: u8) -> u32 {
        for e in &self.chip_volumes[..self.chip_volume_count as usize] {
            if e.chip != chip || e.instance != 0 {
                continue;
            }
            if e.raw & 0x8000 != 0 {
                return (e.raw & 0x7FFF) as u32; // относительная — это и есть множитель
            }
            let idx = (chip & 0x7F) as usize;
            let mut base = *CHIP_BASE_VOL.get(idx).unwrap_or(&0x100) as u32;
            // У YM2203 парная часть (SSG) по умолчанию вдвое тише FM —
            // так считает libvgm, и абсолютное значение надо мерить от
            // этого, иначе поправка выйдет вдвое.
            if chip & 0x80 != 0 && idx == 0x06 {
                base /= 2;
            }
            return if base == 0 { 256 } else { e.raw as u32 * 256 / base };
        }
        256
    }
}

/// Таблица громкостей из extra header (смещение 0xBC, VGM 1.70+).
///
/// Раскладка: сам extra header — размер, смещение таблицы тактовых,
/// смещение таблицы громкостей; оба смещения отсчитываются от своего
/// поля. Таблица громкостей — счётчик и по 4 байта на запись.
///
/// Всё с проверкой границ: заголовок приходит с карты памяти, и падать
/// на кривом файле нельзя.
fn parse_chip_volumes(d: &[u8], hdr_end: usize) -> ([ChipVolume; MAX_CHIP_VOLUMES], u8) {
    let mut out = [ChipVolume::default(); MAX_CHIP_VOLUMES];
    if hdr_end < 0xC0 {
        return (out, 0);
    }
    let rel = rd32(d, 0xBC).unwrap_or(0) as usize;
    if rel == 0 {
        return (out, 0);
    }
    let xo = 0xBC + rel;
    if rd32(d, xo).unwrap_or(0) < 12 {
        return (out, 0); // таблицы громкостей в этом заголовке нет
    }
    let vrel = rd32(d, xo + 8).unwrap_or(0) as usize;
    if vrel == 0 {
        return (out, 0);
    }
    let vb = xo + 8 + vrel;
    let n = match d.get(vb) {
        Some(&n) => (n as usize).min(MAX_CHIP_VOLUMES),
        None => return (out, 0),
    };
    let mut got = 0;
    for i in 0..n {
        let e = vb + 1 + 4 * i;
        match d.get(e..e + 4) {
            Some(b) => {
                out[got] = ChipVolume {
                    chip: b[0],
                    instance: b[1] & 1,
                    raw: b[2] as u16 | (b[3] as u16) << 8,
                };
                got += 1;
            }
            None => break,
        }
    }
    (out, got as u8)
}

/// Итератор по командам VGM. Не владеет данными; позицию можно
/// сохранять/восстанавливать (для лупа).
pub struct Reader<'a> {
    data: &'a [u8],
    pub pos: usize,
    /// указатель чтения DAC-банка YM2612 (команды 0x80-0x8F, seek 0xE0)
    pub dac_ptr: u32,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8], start: usize) -> Reader<'a> {
        Reader { data, pos: start, dac_ptr: 0 }
    }

    fn u8(&mut self) -> Result<u8, Error> {
        let b = *self.data.get(self.pos).ok_or(Error::TooShort)?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16, Error> {
        Ok(self.u8()? as u16 | (self.u8()? as u16) << 8)
    }

    fn skip(&mut self, n: usize) -> Result<(), Error> {
        if self.pos + n > self.data.len() {
            return Err(Error::TooShort);
        }
        self.pos += n;
        Ok(())
    }

    /// Следующее событие потока.
    pub fn next_event(&mut self) -> Result<Event, Error> {
        let at = self.pos;
        let cmd = self.u8()?;
        let ev = match cmd {
            0x50 => Event::Write { chip: Chip::Sn76489, port: 0, addr: 0, data: self.u8()? },
            // Второй SN76489. На Neo Geo Pocket это не второй чип, а
            // вторая половина T6W28: стороны стерео с раздельной
            // громкостью. Все файлы NGP падали здесь с UnknownCommand.
            0x30 => Event::Write { chip: Chip::Sn76489, port: 1, addr: 0, data: self.u8()? },
            // Маска стерео Game Gear — отдельная команда, не запись в
            // регистр: раньше 0x4F был склеен с 0x50 и глушил канал шума
            // (подробности у Event::GgStereo).
            0x4F => Event::GgStereo { chip2: false, mask: self.u8()? },
            0x3F => Event::GgStereo { chip2: true, mask: self.u8()? },
            0x51 => self.reg_write(Chip::Ym2413, 0)?,
            0x52 => self.reg_write(Chip::Ym2612, 0)?,
            0x53 => self.reg_write(Chip::Ym2612, 1)?,
            0x54 => self.reg_write(Chip::Ym2151, 0)?,
            // YM2203 — один порт; YM2608 — два
            0x55 | 0x56 => self.reg_write(Chip::Opn, 0)?,
            0x57 => self.reg_write(Chip::Opn, 1)?,
            0xB9 => self.reg_write(Chip::HuC6280, 0)?,
            // OPL-семейство на нашем OPL3: YM3812/YM3526 — один банк,
            // YMF262 — два порта (0x5E/0x5F)
            0x5A | 0x5B | 0x5E => self.reg_write(Chip::Opl, 0)?,
            0x5F => self.reg_write(Chip::Opl, 1)?,
            0x55..=0x5D => {
                // прочие FM-чипы: пропускаем, сохраняя формат
                self.skip(2)?;
                Event::Write { chip: Chip::Unknown(cmd), port: 0, addr: 0, data: 0 }
            }
            0x61 => Event::Wait { ticks: self.u16()? },
            0x62 => Event::Wait { ticks: 735 },
            0x63 => Event::Wait { ticks: 882 },
            0x66 => Event::End,
            0x67 => {
                self.u8()?; // 0x66 (совместимость)
                let kind = self.u8()?;
                let len = (rd32(self.data, self.pos)? & 0x7FFF_FFFF) as usize;
                self.pos += 4;
                let start = self.pos;
                self.skip(len)?;
                Event::DataBlock { kind, start, len }
            }
            0x68 => {
                self.skip(11)?;
                Event::Write { chip: Chip::Unknown(cmd), port: 0, addr: 0, data: 0 }
            }
            0x70..=0x7F => Event::Wait { ticks: (cmd & 0xF) as u16 + 1 },
            0x80..=0x8F => {
                let off = self.dac_ptr;
                self.dac_ptr += 1;
                Event::Ym2612Dac { ticks: cmd & 0xF, offset: off }
            }
            0x90..=0x95 => {
                const LEN: [usize; 6] = [4, 4, 5, 10, 1, 4];
                let len = LEN[(cmd - 0x90) as usize];
                let start = self.pos;
                self.skip(len)?;
                Event::DacStream { cmd, start, len }
            }
            // Чипы с коротким регистровым полем: бит 7 байта регистра —
            // это признак ВТОРОГО экземпляра, а не часть адреса. Раньше
            // он проходил в чип как есть.
            0xA0 => self.reg_write_dual(Chip::Ay8910, 0, cmd)?,
            0xB4 => self.reg_write_dual(Chip::NesApu, 0, cmd)?,
            0xB3 => self.reg_write_dual(Chip::GbDmg, 0, cmd)?,
            0xB7 => self.reg_write_dual(Chip::Okim6258, 0, cmd)?,
            0xB8 => self.reg_write_dual(Chip::Okim6295, 0, cmd)?,
            0xBA => self.reg_write_dual(Chip::K053260, 0, cmd)?,
            // Второй экземпляр FM-чипов: 0xA1-0xAF — зеркало 0x51-0x5F.
            // Регистровое поле у них полные 8 бит (0xB0-0xB6 — это
            // панорама YM2612), поэтому битом 7 второй чип там выбрать
            // нельзя, и спецификация отвела отдельные команды. Раньше вся
            // эта полоса молча уходила в «пропустить и забыть».
            0xA1..=0xAF => {
                self.skip(2)?;
                Event::SecondChip { cmd }
            }
            0xB0..=0xB2 | 0xB5 | 0xB6 | 0xBB..=0xBF => {
                self.skip(2)?;
                Event::Write { chip: Chip::Unknown(cmd), port: 0, addr: 0, data: 0 }
            }
            0xC0 => {
                let offset = self.u16()?;
                let data = self.u8()?;
                // У SegaPCM второй экземпляр выбирается старшим битом
                // 16-битного смещения
                if offset & 0x8000 != 0 {
                    Event::SecondChip { cmd }
                } else {
                    Event::SegaPcmWrite { offset, data }
                }
            }
            0xD2 => {
                // K051649/K052539 (SCC): pp aa dd — порт, регистр, данные.
                // Второй экземпляр — бит 7 байта порта.
                let port = self.u8()?;
                let addr = self.u8()?;
                let data = self.u8()?;
                if port & 0x80 != 0 {
                    Event::SecondChip { cmd }
                } else {
                    Event::Write { chip: Chip::K051649, port, addr, data }
                }
            }
            0xC1..=0xDF => {
                self.skip(3)?;
                Event::Write { chip: Chip::Unknown(cmd), port: 0, addr: 0, data: 0 }
            }
            0xE0 => {
                // seek указателя DAC-банка
                let a = rd32(self.data, self.pos)?;
                self.pos += 4;
                self.dac_ptr = a;
                Event::Write { chip: Chip::Unknown(cmd), port: 0, addr: 0, data: 0 }
            }
            0xE1..=0xFF => {
                self.skip(4)?;
                Event::Write { chip: Chip::Unknown(cmd), port: 0, addr: 0, data: 0 }
            }
            _ => return Err(Error::UnknownCommand { cmd, pos: at }),
        };
        Ok(ev)
    }

    fn reg_write(&mut self, chip: Chip, port: u8) -> Result<Event, Error> {
        Ok(Event::Write { chip, port, addr: self.u8()?, data: self.u8()? })
    }

    /// То же, но для чипов, у которых регистровое поле короче байта: там
    /// бит 7 байта регистра выбирает второй экземпляр чипа. Раньше этот
    /// бит уходил в чип как часть адреса, и записи второго экземпляра
    /// садились на регистры первого.
    fn reg_write_dual(&mut self, chip: Chip, port: u8, cmd: u8) -> Result<Event, Error> {
        let addr = self.u8()?;
        let data = self.u8()?;
        if addr & 0x80 != 0 {
            return Ok(Event::SecondChip { cmd });
        }
        Ok(Event::Write { chip, port, addr, data })
    }
}

/// Если буфер начинается с gzip-магии — распаковывает (.vgz), иначе копирует.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, Error> {
    if data.len() >= 2 && data[0..2] == GZIP_MAGIC {
        // gzip: 10-байтный заголовок (+опциональные поля), deflate, crc+isize
        let flg = *data.get(3).ok_or(Error::Gzip)?;
        let mut p = 10usize;
        if flg & 0x04 != 0 {
            // FEXTRA
            let xlen = *data.get(p).ok_or(Error::Gzip)? as usize
                | (*data.get(p + 1).ok_or(Error::Gzip)? as usize) << 8;
            p += 2 + xlen;
        }
        for bit in [0x08u8, 0x10] {
            // FNAME, FCOMMENT: строки с нулевым байтом
            if flg & bit != 0 {
                while *data.get(p).ok_or(Error::Gzip)? != 0 {
                    p += 1;
                }
                p += 1;
            }
        }
        if flg & 0x02 != 0 {
            p += 2; // FHCRC
        }
        miniz_oxide::inflate::decompress_to_vec(data.get(p..).ok_or(Error::Gzip)?)
            .map_err(|_| Error::Gzip)
    } else {
        Ok(Vec::from(data))
    }
}

/// Метаданные GD3 (title/game/system/author в UTF-16LE).
pub struct Gd3<'a> {
    strings: &'a [u8],
}

impl<'a> Gd3<'a> {
    pub fn parse(d: &'a [u8], gd3_offset: usize) -> Option<Gd3<'a>> {
        let tag = d.get(gd3_offset..gd3_offset + 12)?;
        if &tag[0..4] != b"Gd3 " {
            return None;
        }
        let len = u32::from_le_bytes([tag[8], tag[9], tag[10], tag[11]]) as usize;
        Some(Gd3 { strings: d.get(gd3_offset + 12..gd3_offset + 12 + len)? })
    }

    /// n-я UTF-16LE строка тега (0 = трек EN, 2 = игра EN, 6 = система EN,
    /// 8 = автор EN). Возвращает итератор по code unit'ам.
    pub fn string(&self, n: usize) -> impl Iterator<Item = u16> + '_ {
        let mut skipped = 0usize;
        let mut i = 0usize;
        while skipped < n && i + 1 < self.strings.len() {
            if self.strings[i] == 0 && self.strings[i + 1] == 0 {
                skipped += 1;
            }
            i += 2;
        }
        let tail = &self.strings[i.min(self.strings.len())..];
        tail.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собирает минимальный валидный VGM 1.51 с YM2151.
    fn synth_vgm(body: &[u8], loop_at: Option<usize>) -> Vec<u8> {
        let mut v = alloc::vec![0u8; 0x80];
        v[0..4].copy_from_slice(VGM_MAGIC);
        v[0x08..0x0C].copy_from_slice(&0x0151u32.to_le_bytes());
        v[0x30..0x34].copy_from_slice(&4_000_000u32.to_le_bytes());
        v[0x34..0x38].copy_from_slice(&(0x80u32 - 0x34).to_le_bytes());
        if let Some(off) = loop_at {
            v[0x1C..0x20].copy_from_slice(&((0x80 + off - 0x1C) as u32).to_le_bytes());
        }
        v.extend_from_slice(body);
        let eof = (v.len() - 4) as u32;
        v[0x04..0x08].copy_from_slice(&eof.to_le_bytes());
        v
    }

    #[test]
    fn parses_header_and_commands() {
        let body = [
            0x54, 0x28, 0x4A, // YM2151 reg 0x28 = 0x4A
            0x61, 0xDF, 0x02, // wait 735
            0x73,             // wait 4
            0x66,             // end
        ];
        let d = synth_vgm(&body, None);
        let h = Header::parse(&d).unwrap();
        assert_eq!(h.version, 0x151);
        assert_eq!(h.clocks.ym2151, 4_000_000);
        assert_eq!(h.data_offset, 0x80);
        assert!(h.loop_offset.is_none());

        let mut r = Reader::new(&d, h.data_offset);
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::Ym2151, port: 0, addr: 0x28, data: 0x4A }
        );
        assert_eq!(r.next_event().unwrap(), Event::Wait { ticks: 735 });
        assert_eq!(r.next_event().unwrap(), Event::Wait { ticks: 4 });
        assert_eq!(r.next_event().unwrap(), Event::End);
    }

    #[test]
    fn parses_scc_write() {
        // 0xD2 pp aa dd -> K051649 write (порт, регистр, данные)
        let body = [0xD2, 0x01, 0x00, 0xFD, 0x66];
        let d = synth_vgm(&body, None);
        let h = Header::parse(&d).unwrap();
        let mut r = Reader::new(&d, h.data_offset);
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::K051649, port: 0x01, addr: 0x00, data: 0xFD }
        );
        assert_eq!(r.next_event().unwrap(), Event::End);
    }

    #[test]
    fn parses_k053260_write() {
        // 0xBA aa dd -> K053260 write
        let body = [0xBA, 0x0F, 0x40, 0x66];
        let d = synth_vgm(&body, None);
        let h = Header::parse(&d).unwrap();
        let mut r = Reader::new(&d, h.data_offset);
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::K053260, port: 0, addr: 0x0F, data: 0x40 }
        );
    }

    #[test]
    fn parses_opl_writes() {
        // 0x5A (YM3812) и 0x5F (YMF262 порт 1) -> Chip::Opl с нужным банком
        let body = [0x5A, 0x20, 0x01, 0x5F, 0xA0, 0x44, 0x66];
        let d = synth_vgm(&body, None);
        let h = Header::parse(&d).unwrap();
        let mut r = Reader::new(&d, h.data_offset);
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::Opl, port: 0, addr: 0x20, data: 0x01 }
        );
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::Opl, port: 1, addr: 0xA0, data: 0x44 }
        );
        assert_eq!(r.next_event().unwrap(), Event::End);
    }

    #[test]
    fn data_block_and_loop() {
        let body = [
            0x67, 0x66, 0x80, 0x04, 0x00, 0x00, 0x00, 1, 2, 3, 4, // блок SegaPCM
            0x54, 0x08, 0x00,
            0x66,
        ];
        let d = synth_vgm(&body, Some(11));
        let h = Header::parse(&d).unwrap();
        assert_eq!(h.loop_offset, Some(0x80 + 11));

        let mut r = Reader::new(&d, h.data_offset);
        match r.next_event().unwrap() {
            Event::DataBlock { kind: 0x80, start, len: 4 } => {
                assert_eq!(&d[start..start + 4], &[1, 2, 3, 4]);
            }
            e => panic!("не блок: {e:?}"),
        }
        assert_eq!(r.pos, h.loop_offset.unwrap());
    }

    #[test]
    fn real_files_if_present() {
        // Интеграционный прогон по локальной коллекции (не в репо).
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vgmrips");
        if !dir.exists() {
            return;
        }
        let mut checked = 0;
        for entry in walk(&dir) {
            let raw = std::fs::read(&entry).unwrap();
            let data = decompress(&raw).unwrap();
            let h = Header::parse(&data).unwrap();
            let mut r = Reader::new(&data, h.data_offset);
            let mut ticks = 0u64;
            loop {
                match r.next_event().unwrap() {
                    Event::End => break,
                    Event::Wait { ticks: t } => ticks += t as u64,
                    Event::Ym2612Dac { ticks: t, .. } => ticks += t as u64,
                    _ => {}
                }
            }
            assert_eq!(ticks, h.total_ticks as u64, "{}", entry.display());
            checked += 1;
        }
        assert!(checked > 0);
    }

    #[cfg(test)]
    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else if matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("vgm") | Some("vgz")
            ) {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn parses_opn_writes() {
        // 0x55 YM2203, 0x56/0x57 YM2608 порт 0/1 -> Chip::Opn
        let body = [0x55, 0x07, 0x38, 0x56, 0x00, 0xFE, 0x57, 0xB4, 0xC0, 0x66];
        let d = synth_vgm(&body, None);
        let h = Header::parse(&d).unwrap();
        let mut r = Reader::new(&d, h.data_offset);
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::Opn, port: 0, addr: 0x07, data: 0x38 }
        );
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::Opn, port: 0, addr: 0x00, data: 0xFE }
        );
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::Opn, port: 1, addr: 0xB4, data: 0xC0 }
        );
        assert_eq!(r.next_event().unwrap(), Event::End);
    }

    /// Смещения полей клока в заголовке. Ловится только так: у HuC6280
    /// было записано 0xA5 вместо 0xA4, и промах на байт давал 14 кГц
    /// вместо 3.58 МГц — заметил лишь на настоящем рипе PC Engine.
    #[test]
    fn clock_fields_sit_at_documented_offsets() {
        let mut v = alloc::vec![0u8; 0x100];
        v[0..4].copy_from_slice(VGM_MAGIC);
        v[0x08..0x0C].copy_from_slice(&0x0161u32.to_le_bytes());
        // заголовок до 0x100, чтобы поля за 0x80 попали внутрь
        v[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
        let fields: [(usize, u32); 8] = [
            (0x0C, 3_579_545),  // SN76489
            (0x2C, 7_670_453),  // YM2612
            (0x30, 4_000_000),  // YM2151
            (0x44, 3_993_600),  // YM2203
            (0x48, 7_987_200),  // YM2608
            (0x5C, 14_318_180), // YMF262
            (0x9C, 1_789_772),  // K051649
            (0xA4, 3_579_545),  // HuC6280
        ];
        for (off, val) in fields {
            v[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }
        v.push(0x66);
        let eof = (v.len() - 4) as u32;
        v[0x04..0x08].copy_from_slice(&eof.to_le_bytes());

        let c = Header::parse(&v).unwrap().clocks;
        assert_eq!(c.sn76489, 3_579_545);
        assert_eq!(c.ym2612, 7_670_453);
        assert_eq!(c.ym2151, 4_000_000);
        assert_eq!(c.ym2203, 3_993_600);
        assert_eq!(c.ym2608, 7_987_200);
        assert_eq!(c.ymf262, 14_318_180);
        assert_eq!(c.k051649, 1_789_772);
        assert_eq!(c.huc6280, 3_579_545);
    }

    /// Чипы, которые мы не играем, но обязаны назвать. Смещения сняты с
    /// настоящих файлов из архива, а не со спецификации: PWM у 32X,
    /// uPD7759 у Sega Pico, WonderSwan у своей приставки. Флаг дисковой
    /// приставки Famicom живёт в старшем бите поля NES APU, который
    /// clock_field снимает, — проверяем, что он всё-таки виден.
    #[test]
    fn declared_but_silent_chips_are_recognised() {
        let mut v = alloc::vec![0u8; 0x100];
        v[0..4].copy_from_slice(VGM_MAGIC);
        v[0x08..0x0C].copy_from_slice(&0x0171u32.to_le_bytes());
        v[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
        v[0x70..0x74].copy_from_slice(&23_011_360u32.to_le_bytes());
        v[0x8C..0x90].copy_from_slice(&0x800F_4240u32.to_le_bytes());
        v[0xC0..0xC4].copy_from_slice(&3_072_000u32.to_le_bytes());
        v[0x84..0x88].copy_from_slice(&0x801B_4F4Du32.to_le_bytes());
        v.push(0x66);
        let eof = (v.len() - 4) as u32;
        v[0x04..0x08].copy_from_slice(&eof.to_le_bytes());

        let c = Header::parse(&v).unwrap().clocks;
        assert_eq!(c.pwm, 23_011_360);
        assert_eq!(c.upd7759, 1_000_000);
        assert_eq!(c.wonderswan, 3_072_000);
        assert!(c.fds);
        // сам APU остаётся с честной частотой, без флага в старшем бите
        assert_eq!(c.nes_apu, 1_789_773);
    }

    /// Маска стерео Game Gear — не запись в регистр PSG.
    ///
    /// Пока 0x4F был склеен с 0x50, маска уходила в чип как данные, а
    /// обычное её значение 0xFF для SN76489 означает latch «канал 3,
    /// громкость, аттенюация 15», то есть глушит шум. Проверяем именно
    /// это значение.
    #[test]
    fn gg_stereo_is_not_a_psg_write() {
        let body = [0x4F, 0xFF, 0x50, 0x9F, 0x3F, 0x11, 0x66];
        let d = synth_vgm(&body, None);
        let h = Header::parse(&d).unwrap();
        let mut r = Reader::new(&d, h.data_offset);
        assert_eq!(r.next_event().unwrap(), Event::GgStereo { chip2: false, mask: 0xFF });
        // настоящая запись в PSG идёт следом и не пострадала
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::Sn76489, port: 0, addr: 0, data: 0x9F }
        );
        assert_eq!(r.next_event().unwrap(), Event::GgStereo { chip2: true, mask: 0x11 });
        assert_eq!(r.next_event().unwrap(), Event::End);
    }

    /// Записи второго экземпляра чипа не должны доставаться первому.
    #[test]
    fn second_chip_writes_do_not_reach_the_first() {
        let body = [
            0xA0, 0x07, 0x38, // AY: регистр 7 первого чипа
            0xA0, 0x87, 0x38, // AY: тот же регистр ВТОРОГО чипа (бит 7)
            0xA2, 0x28, 0x00, // второй YM2612, порт 0 (зеркало 0x52)
            0xB4, 0x95, 0x1F, // второй NES APU (бит 7 в байте регистра)
            0x66,
        ];
        let d = synth_vgm(&body, None);
        let h = Header::parse(&d).unwrap();
        let mut r = Reader::new(&d, h.data_offset);
        assert_eq!(
            r.next_event().unwrap(),
            Event::Write { chip: Chip::Ay8910, port: 0, addr: 0x07, data: 0x38 }
        );
        assert_eq!(r.next_event().unwrap(), Event::SecondChip { cmd: 0xA0 });
        assert_eq!(r.next_event().unwrap(), Event::SecondChip { cmd: 0xA2 });
        assert_eq!(r.next_event().unwrap(), Event::SecondChip { cmd: 0xB4 });
        assert_eq!(r.next_event().unwrap(), Event::End);
    }

    /// Громкости чипов из extra header. Значения взяты из настоящих
    /// файлов корпуса: рипы YM2203 просят SSG в 1.602 раза ГРОМЧЕ
    /// умолчания, рипы YM2608 — в 0.625 раза тише. Игнорировать это
    /// значит слушать другой баланс FM и SSG, чем задумал автор рипа.
    #[test]
    fn chip_volumes_from_extra_header() {
        let mut v = alloc::vec![0u8; 0x100];
        v[0..4].copy_from_slice(VGM_MAGIC);
        v[0x08..0x0C].copy_from_slice(&0x0171u32.to_le_bytes());
        v[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
        v[0x44..0x48].copy_from_slice(&3_993_600u32.to_le_bytes()); // YM2203
        // extra header по 0xC0: размер 12, тактовых нет, громкости +4
        v[0xBC..0xC0].copy_from_slice(&(0xC0u32 - 0xBC).to_le_bytes());
        v[0xC0..0xC4].copy_from_slice(&12u32.to_le_bytes());
        v[0xC4..0xC8].copy_from_slice(&0u32.to_le_bytes());
        v[0xC8..0xCC].copy_from_slice(&4u32.to_le_bytes());
        // Таблица лежит по xo + 8 + смещение = 0xC0 + 8 + 4 = 0xCC, то
        // есть ВНУТРИ заголовка, а не за ним: одна запись, SSG YM2203
        // (0x86), относительная 0x819A
        v[0xCC..0xD1].copy_from_slice(&[1, 0x86, 0x00, 0x9A, 0x81]);
        v.push(0x66);
        let eof = (v.len() - 4) as u32;
        v[0x04..0x08].copy_from_slice(&eof.to_le_bytes());

        let h = Header::parse(&v).unwrap();
        assert_eq!(h.chip_volume_count, 1);
        assert_eq!(h.chip_volumes[0], ChipVolume { chip: 0x86, instance: 0, raw: 0x819A });
        assert_eq!(h.chip_scale(0x86), 0x19A); // 410/256 = 1.602x
        assert_eq!(h.chip_scale(0x06), 256); // FM-часть не тронута
        assert_eq!(h.chip_scale(0x12), 256); // чужой чип тоже
        assert_eq!(h.volume_scale(), 256); // модификатор не задан
    }

    /// Общий модификатор громкости: множитель 2^(v/32), знак разворачивается
    /// как в libvgm, вместе с особым случаем 0xC1.
    #[test]
    fn volume_modifier_matches_reference() {
        let mut v = synth_vgm(&[0x66], None);
        let set = |v: &mut alloc::vec::Vec<u8>, b: u8| {
            v[0x7C] = b;
            Header::parse(v).unwrap()
        };
        assert_eq!(set(&mut v, 0x00).volume_scale(), 256); // без изменений
        assert_eq!(set(&mut v, 0x20).volume_scale(), 512); // ровно вдвое
        assert_eq!(set(&mut v, 0x40).volume_scale(), 1024); // вчетверо
        assert_eq!(set(&mut v, 0xE0).volume_scale(), 128); // -0x20 -> вдвое тише
        // 0xC1 у libvgm даёт -0x40, а не -0x3F: 2^-2 = 0.25
        assert_eq!(set(&mut v, 0xC1).volume_scale(), 64);
        assert_eq!(set(&mut v, 0x10).volume_modifier, 0x10);
        assert_eq!(set(&mut v, 0xFF).volume_modifier, -1);
    }

    /// Признак второго экземпляра в заголовке — бит 30 поля тактовой. У
    /// SN76489 он же вместе с битом 31 означает T6W28, а это ОДИН чип.
    #[test]
    fn second_chip_flag_separates_t6w28() {
        let mut v = synth_vgm(&[0x66], None);
        assert!(!Header::parse(&v).unwrap().clocks.second_chip);

        // второй YM2612
        v[0x2C..0x30].copy_from_slice(&(7_670_453u32 | 0x4000_0000).to_le_bytes());
        assert!(Header::parse(&v).unwrap().clocks.second_chip);
        v[0x2C..0x30].copy_from_slice(&0u32.to_le_bytes());

        // T6W28 (Neo Geo Pocket): биты 30 и 31 сразу — вторым чипом не считается
        v[0x0C..0x10].copy_from_slice(&(3_072_000u32 | 0xC000_0000).to_le_bytes());
        let c = Header::parse(&v).unwrap().clocks;
        assert!(c.sn_t6w28 && c.sn_dual);
        assert!(!c.second_chip);

        // а один бит 30 у SN76489 — это уже настоящий второй чип
        v[0x0C..0x10].copy_from_slice(&(3_579_545u32 | 0x4000_0000).to_le_bytes());
        assert!(Header::parse(&v).unwrap().clocks.second_chip);
    }
}
