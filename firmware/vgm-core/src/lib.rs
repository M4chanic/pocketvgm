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
}

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
}

fn rd32(d: &[u8], off: usize) -> Result<u32, Error> {
    let b = d.get(off..off + 4).ok_or(Error::TooShort)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
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
            huc6280: clock_field(d, 0xA5, hdr_end),
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
        };

        Ok(Header {
            version,
            total_ticks: rd32(d, 0x18)?,
            loop_offset,
            loop_ticks: rd32(d, 0x20)?,
            data_offset,
            gd3_offset,
            clocks,
        })
    }
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
            0x4F | 0x50 => Event::Write { chip: Chip::Sn76489, port: 0, addr: 0, data: self.u8()? },
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
            0xA0 => self.reg_write(Chip::Ay8910, 0)?,
            0xB4 => self.reg_write(Chip::NesApu, 0)?,
            0xB3 => self.reg_write(Chip::GbDmg, 0)?,
            0xB7 => self.reg_write(Chip::Okim6258, 0)?,
            0xB8 => self.reg_write(Chip::Okim6295, 0)?,
            0xBA => self.reg_write(Chip::K053260, 0)?,
            0xA1..=0xB2 | 0xB5 | 0xB6 | 0xB9..=0xBF => {
                self.skip(2)?;
                Event::Write { chip: Chip::Unknown(cmd), port: 0, addr: 0, data: 0 }
            }
            0xC0 => {
                let offset = self.u16()?;
                Event::SegaPcmWrite { offset, data: self.u8()? }
            }
            0xD2 => {
                // K051649/K052539 (SCC): pp aa dd — порт, регистр, данные
                let port = self.u8()?;
                let addr = self.u8()?;
                let data = self.u8()?;
                Event::Write { chip: Chip::K051649, port, addr, data }
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
}
