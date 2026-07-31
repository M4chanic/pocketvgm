//! Минимальный экранный UI: текст 8x8 (x2) в RGB565-фреймбуфер litex.

use crate::font::FONT8X8;

const FB_BASE: *mut u16 = 0x40C0_0000 as *mut u16;
const W: usize = 266;
const H: usize = 240;

pub const BG: u16 = 0x0841; // почти чёрный с синевой
pub const FG: u16 = 0xFFFF;
pub const ACCENT: u16 = 0xFD20; // оранжевый
pub const DIM: u16 = 0x8410; // серый

/// Включение видеотракта (VTG + DMA на наш буфер)
pub fn init() {
    let p = unsafe { litex_openfpga::litex_pac::Peripherals::steal() };
    unsafe {
        p.VIDEO_FRAMEBUFFER_VTG.enable.write(|w| w.bits(0));
        p.VIDEO_FRAMEBUFFER.dma_enable.write(|w| w.bits(0));
        p.VIDEO_FRAMEBUFFER.dma_base.write(|w| w.bits(FB_BASE as u32));
        p.VIDEO_FRAMEBUFFER_VTG.enable.write(|w| w.bits(1));
        p.VIDEO_FRAMEBUFFER.dma_enable.write(|w| w.bits(1));
    }
    clear();
}

pub fn clear() {
    for i in 0..W * H {
        unsafe { FB_BASE.add(i).write_volatile(BG) };
    }
}

fn put_px(x: usize, y: usize, c: u16) {
    if x < W && y < H {
        unsafe { FB_BASE.add(y * W + x).write_volatile(c) };
    }
}

/// Текст 8x8 с масштабом (1 или 2), возвращает ширину в пикселях
pub fn text(x: usize, y: usize, s: &str, color: u16, scale: usize) -> usize {
    let mut cx = x;
    for ch in s.chars() {
        let idx = if (' '..='\u{7f}').contains(&ch) { ch as usize - 32 } else { 0x3F - 32 };
        let glyph = &FONT8X8[idx];
        for (ry, row) in glyph.iter().enumerate() {
            for rx in 0..8 {
                if row >> rx & 1 != 0 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            put_px(cx + rx * scale + sx, y + ry * scale + sy, color);
                        }
                    }
                }
            }
        }
        cx += 8 * scale;
        if cx >= W {
            break;
        }
    }
    cx - x
}

fn hline(y: usize, c: u16) {
    for x in 8..W - 8 {
        put_px(x, y, c);
    }
}

/// Строка статуса (PAUSED/STOPPED) в отведённой полосе; пустая — очистить
const STATUS_Y: usize = 164;

pub fn status(s: &str) {
    for y in STATUS_Y..STATUS_Y + 16 {
        for x in 0..W {
            put_px(x, y, BG);
        }
    }
    if !s.is_empty() {
        text(12, STATUS_Y, s, ACCENT, 2);
    }
}

/// Полоса времени/прогресса: cur/total в секундах (total=0 — без полосы
/// и без правой части), diag — короткая строка диагностики справа
const PROG_Y: usize = 138;

fn fmt_time(buf: &mut [u8; 8], s: u32) -> &str {
    let m = (s / 60).min(99);
    let sec = s % 60;
    buf[0] = b'0' + (m / 10) as u8;
    buf[1] = b'0' + (m % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (sec / 10) as u8;
    buf[4] = b'0' + (sec % 10) as u8;
    core::str::from_utf8(&buf[..5]).unwrap_or("?")
}

pub fn progress(cur: u32, total: u32, diag: &str) {
    for y in PROG_Y..PROG_Y + 22 {
        for x in 0..W {
            put_px(x, y, BG);
        }
    }
    let mut b1 = [0u8; 8];
    let mut x = 12 + text(12, PROG_Y, fmt_time(&mut b1, cur), FG, 1);
    if total > 0 {
        x += text(x, PROG_Y, " / ", DIM, 1);
        let mut b2 = [0u8; 8];
        text(x, PROG_Y, fmt_time(&mut b2, total), DIM, 1);
        // полоса под текстом
        let bw = W - 24;
        let fill = (bw as u64 * cur.min(total) as u64 / total.max(1) as u64) as usize;
        for y in PROG_Y + 13..PROG_Y + 17 {
            for px in 0..bw {
                put_px(12 + px, y, if px < fill { ACCENT } else { DIM });
            }
        }
    }
    if !diag.is_empty() {
        let w = diag.len() * 8;
        text(W - 12 - w, PROG_Y, diag, DIM, 1);
    }
}

/// Стерео VU-метр (пики 0..32767) в полосе между статусом и подсказками
const VU_Y: usize = 180;

pub fn vu(l: u16, r: u16) {
    for y in VU_Y..VU_Y + 15 {
        for x in 0..W {
            put_px(x, y, BG);
        }
    }
    let maxw = W - 40;
    text(12, VU_Y - 1, "L", DIM, 1);
    text(12, VU_Y + 7, "R", DIM, 1);
    let lw = l as usize * maxw / 32768;
    let rw = r as usize * maxw / 32768;
    for y in VU_Y + 1..VU_Y + 5 {
        for x in 0..lw {
            put_px(26 + x, y, ACCENT);
        }
    }
    for y in VU_Y + 9..VU_Y + 13 {
        for x in 0..rw {
            put_px(26 + x, y, ACCENT);
        }
    }
}

/// Браузер плейлиста: список с курсором, '*' у играющего трека
pub fn browser(names: &[&str], cursor: usize, playing: usize, title: &str) {
    clear();
    text(12, 8, title, ACCENT, 1);
    let rows = 20usize;
    let top = cursor.saturating_sub(rows / 2)
        .min(names.len().saturating_sub(rows));
    for (row, i) in (top..names.len().min(top + rows)).enumerate() {
        let y = 22 + row * 10;
        if i == cursor {
            text(2, y, ">", ACCENT, 1);
        }
        let mark = if i == playing { ACCENT } else if i == cursor { FG } else { DIM };
        text(12, y, names[i], mark, 1);
    }
    text(12, 228, "^ v move   A/> play   B/< back", DIM, 1);
}

/// Полный экран плеера
/// Титульный экран: показывается, когда файл НЕ выбирался пользователем
/// явно (иначе плеер сразу играет выбранное). Без него ядро молча
/// начинало проигрывать прошлый или случайный слот.
pub fn title(version: &str) {
    clear();

    text(12, 44, "PocketVGM", ACCENT, 2);
    text(12, 66, version, DIM, 1);
    hline(84, DIM);

    text(12, 104, "To play music:", FG, 1);
    text(12, 124, "open the Pocket menu,", DIM, 1);
    text(12, 138, "choose Load, pick a file.", DIM, 1);
}

/// Разбить строку на две по max байт, не разрезая символ UTF-8.
/// Резать строку срезом нельзя: на многобайтовом символе это паника.
fn split_fit(s: &str, max: usize) -> (&str, &str) {
    if s.len() <= max {
        return (s, "");
    }
    let b = s.as_bytes();
    let mut cut = max;
    while cut > 0 && (b[cut] & 0xC0) == 0x80 {
        cut -= 1;
    }
    // по возможности рвём по пробелу, чтобы слово не разваливалось
    let brk = b[..cut].iter().rposition(|&c| c == b' ').unwrap_or(cut);
    let (a, rest) = s.split_at(if brk + 8 >= cut { brk } else { cut });
    let rest = rest.trim_start();
    let mut end = rest.len().min(max);
    let rb = rest.as_bytes();
    while end > 0 && end < rest.len() && (rb[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    (a.trim_end(), &rest[..end])
}

pub fn screen(
    format: &str,
    title: &str,
    sub: &str,
    system: &str,
    chips: &str,
    song: Option<(u8, u8)>,
    track: Option<(usize, usize)>,
) {
    clear();

    // формат слева, позиция в плейлисте с подписью — справа
    text(12, 12, format, FG, 2);
    if let Some((cur, total)) = track {
        let mut buf = [0u8; 16];
        let s = fmt_pair(&mut buf, cur as u32 + 1, total as u32);
        let w = 8 * (s.len() + 6);
        text(W - 16 - w, 16, "track ", DIM, 1);
        text(W - 16 - 8 * s.len(), 16, s, FG, 1);
    }
    hline(36, DIM);

    // Название и подпись делят три строки: 46 / 58 / 70. Ниже — фикс:
    // system с 88, а прогресс-бар с 138, двигать нечего. Поэтому если
    // название уложилось в строку, вторую отдаём подписи — иначе
    // «Игра - Композитор» обрезалось ровно на 30 знаках.
    let max_chars = (W - 24) / 8;
    let (t1, t2) = split_fit(title, max_chars);
    text(12, 46, t1, FG, 1);
    let sub_y = if t2.is_empty() {
        58
    } else {
        text(12, 58, t2, FG, 1);
        70
    };
    let (s1, s2) = split_fit(sub, max_chars);
    text(12, sub_y, s1, DIM, 1);
    if !s2.is_empty() && sub_y == 58 {
        text(12, 70, s2, DIM, 1);
    }

    text(12, 88, "system:", DIM, 1);
    text(12 + 64, 88, system, FG, 1);
    text(12, 102, "chips:", DIM, 1);
    // Под строку чипов после подписи остаётся всего (266-76-12)/8 = 22
    // знака, а «YM2612+SN76489 (no RF5C164)» это 27 — с устройства пришло
    // «сообщения появились, но не везде влезают». Хвост переносим на
    // следующую строку во всю ширину, там помещается 31 знак.
    //
    // Перенос только когда номера подпесни нет: она рисуется вдвое
    // крупнее с y=116 и заняла бы это место. У форматов с подпеснями
    // (NSF, GBS) длинных строк чипов не бывает — там максимум «(no FDS)».
    let chip_w = (W - 76 - 12) / 8;
    if song.is_none() && chips.len() > chip_w {
        let (c1, c2) = split_fit(chips, chip_w);
        text(12 + 64, 102, c1, FG, 1);
        text(12, 114, c2, FG, 1);
    } else {
        let cut = chips.len().min(chip_w);
        let mut cut = cut;
        while cut > 0 && !chips.is_char_boundary(cut) {
            cut -= 1;
        }
        text(12 + 64, 102, &chips[..cut], FG, 1);
    }

    if let Some((cur, total)) = song {
        let mut buf = [0u8; 16];
        let s = fmt_pair(&mut buf, cur as u32, total as u32);
        text(12, 120, "song:", DIM, 1);
        text(12 + 64, 116, s, ACCENT, 2);
    }

    // подсказки по кнопкам
    if song.is_some() {
        text(12, 196, "< > track  ^ v song  sel: list", DIM, 1);
    } else {
        text(12, 196, "< > track  sel: list", DIM, 1);
    }
    text(12, 210, "A pause   B stop   R ffwd", DIM, 1);
    text(12, 226, "menu: core settings > music", DIM, 1);
    // активный data-слот (диагностика «какой Load сработал»)
    let sn = match crate::files::slot() {
        2 => "s2",
        3 => "s3",
        _ => "s1",
    };
    text(W - 12 - 16, 226, sn, DIM, 1);
}

fn put_num(buf: &mut [u8; 16], n: &mut usize, v: u32) {
    let mut div = 1;
    while v / div >= 10 && div < 1000 {
        div *= 10;
    }
    while div > 0 {
        buf[*n] = b'0' + (v / div % 10) as u8;
        *n += 1;
        div /= 10;
    }
}

fn fmt_pair(buf: &mut [u8; 16], cur: u32, total: u32) -> &str {
    let mut n = 0;
    put_num(buf, &mut n, cur.min(9999));
    buf[n] = b'/';
    n += 1;
    put_num(buf, &mut n, total.min(9999));
    core::str::from_utf8(&buf[..n]).unwrap_or("?")
}
