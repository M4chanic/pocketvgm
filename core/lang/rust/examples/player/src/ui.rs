//! Минимальный экранный UI: текст 8x8 (x2) в RGB565-фреймбуфер litex.

use crate::font::FONT8X8;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

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

// ----------------------------------------------------------------------
// Прокрутка последней строки подписи.
//
// В две строки укладывается 88.7% корпуса (705 файлов с тегами GD3).
// Остаток — в основном игры Konami, где через запятую перечислены три-
// четыре композитора: «Contra: Hard Corps - Hiroshi Kobayashi, Michiru
// Yamane, Akira Ya...» это 99 знаков. Третья статическая строка подняла
// бы покрытие лишь до 92.3%, а места стоит столько же, сколько первые
// две, поэтому вместо неё окно едет по строке.
//
// Шаг — знак, а не точка: put_px отсекает всё за правым краем, а
// координаты беззнаковые, так что уехать влево нечем. Для чтения этого
// достаточно, и обходится без клиппинга.
struct ScrollBuf(UnsafeCell<[u8; 192]>);
// Прошивка однопоточная, доступ только из тика отрисовки
unsafe impl Sync for ScrollBuf {}
static SCROLL: ScrollBuf = ScrollBuf(UnsafeCell::new([0; 192]));
static SCROLL_LEN: AtomicUsize = AtomicUsize::new(0);
static SCROLL_POS: AtomicUsize = AtomicUsize::new(0);
static SCROLL_HOLD: AtomicUsize = AtomicUsize::new(0);
static SCROLL_Y: AtomicUsize = AtomicUsize::new(0);

/// Пауза на краях, в тиках. Тик приходит 12 раз в секунду (см. vu_tick),
/// то есть края держатся полторы секунды — успеть прочитать начало.
const SCROLL_HOLD_TICKS: usize = 18;

fn scroll_set(s: &str, y: usize) {
    let b = unsafe { &mut *SCROLL.0.get() };
    let n = s.len().min(b.len());
    // режем по границе символа, иначе from_utf8 потом откажет
    let mut n = n;
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    b[..n].copy_from_slice(&s.as_bytes()[..n]);
    SCROLL_LEN.store(n, Relaxed);
    SCROLL_POS.store(0, Relaxed);
    SCROLL_HOLD.store(SCROLL_HOLD_TICKS, Relaxed);
    SCROLL_Y.store(y, Relaxed);
}

fn scroll_clear() {
    SCROLL_LEN.store(0, Relaxed);
}

/// Перерисовывает только свою строку — как progress() чистит свою полосу
fn band(y: usize, s: &str, color: u16) {
    for yy in y..y + 8 {
        for x in 12..W {
            put_px(x, yy, BG);
        }
    }
    text(12, y, s, color, 1);
}

/// Двигает окно на знак. Зовётся из того же тика, что и счётчик времени.
pub fn scroll_tick() {
    let len = SCROLL_LEN.load(Relaxed);
    if len == 0 {
        return;
    }
    let buf = unsafe { &*SCROLL.0.get() };
    let s = match core::str::from_utf8(&buf[..len]) {
        Ok(v) => v,
        Err(_) => return,
    };
    let max = (W - 24) / 8;
    let total = s.chars().count();
    if total <= max {
        return;
    }
    let last = total - max;
    let hold = SCROLL_HOLD.load(Relaxed);
    if hold > 0 {
        SCROLL_HOLD.store(hold - 1, Relaxed);
        return;
    }
    let mut pos = SCROLL_POS.load(Relaxed);
    if pos >= last {
        pos = 0;
        SCROLL_HOLD.store(SCROLL_HOLD_TICKS, Relaxed);
    } else {
        pos += 1;
        if pos == last {
            SCROLL_HOLD.store(SCROLL_HOLD_TICKS, Relaxed);
        }
    }
    SCROLL_POS.store(pos, Relaxed);

    let start = s.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(0);
    let win = &s[start..];
    let end = win.char_indices().nth(max).map(|(i, _)| i).unwrap_or(win.len());
    band(SCROLL_Y.load(Relaxed), &win[..end], DIM);
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

    // Шапка ужата в одну строку обычного кегля. Раньше формат стоял
    // вдвое крупнее и занимал полосу 12..36 целиком; полученные оттуда
    // две строки розданы полям, которые обрезались.
    //
    // Сам формат остался, только мелким: на нём же держатся экраны
    // «PANIC» и «?», по которым видно, что случилось.
    text(12, 12, format, DIM, 1);
    if let Some((cur, total)) = track {
        let mut buf = [0u8; 16];
        let s = fmt_pair(&mut buf, cur as u32 + 1, total as u32);
        let w = 8 * (s.len() + 6);
        text(W - 16 - w, 12, "track ", DIM, 1);
        text(W - 16 - 8 * s.len(), 12, s, FG, 1);
    }
    hline(26, DIM);

    // Раскладка посчитана по корпусу: 705 файлов VGM с тегами GD3, в
    // строку влезает 30 знаков (у системы 22 после метки).
    //
    //                        1 строка  2 строки  3 строки
    //   название               86.2%     99.9%    100.0%
    //   подпись «игра-автор»   41.3%     88.7%     92.3%
    //   система                80.1%     99.7%     99.7%
    //
    // Отсюда по две строки на каждое: третья строка подписи добавила бы
    // всего 3.6%, а места стоит столько же. Оставшиеся 11% подписей —
    // под прокрутку последней строки, её тут пока нет.
    let max_chars = (W - 24) / 8;
    let (t1, t2) = split_fit(title, max_chars);
    text(12, 34, t1, FG, 1);
    if !t2.is_empty() {
        text(12, 46, t2, FG, 1);
    }
    let (s1, s2) = split_fit(sub, max_chars);
    text(12, 58, s1, DIM, 1);
    if !s2.is_empty() {
        text(12, 70, s2, DIM, 1);
    }
    // Если и в две строки не влезло — вторую отдаём под прокрутку.
    // s1 обрезан по границе символа, поэтому срез по его длине законен.
    let rest = sub[s1.len()..].trim_start();
    if rest.chars().count() > max_chars {
        scroll_set(rest, 70);
    } else {
        scroll_clear();
    }

    // Система переносится так же. Раньше она рисовалась одной строкой и
    // просто обрезалась — у каждого пятого файла, а «Sega Mega Drive /
    // Genesis» не влезает уже при 25 знаках.
    let sys_w = (W - 76 - 12) / 8;
    let (y1, y2) = split_fit(system, sys_w);
    text(12, 84, "system:", DIM, 1);
    text(12 + 64, 84, y1, FG, 1);
    if !y2.is_empty() {
        text(12, 96, y2, FG, 1);
    }
    text(12, 108, "chips:", DIM, 1);
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
        text(12 + 64, 108, c1, FG, 1);
        text(12, 120, c2, FG, 1);
    } else {
        let cut = chips.len().min(chip_w);
        let mut cut = cut;
        while cut > 0 && !chips.is_char_boundary(cut) {
            cut -= 1;
        }
        text(12 + 64, 108, &chips[..cut], FG, 1);
    }

    // Номер подпесни крупный, занимает 120..136 — впритык к прогресс-бару
    // на 138. Поэтому строка чипов при нём не переносится (см. выше).
    if let Some((cur, total)) = song {
        let mut buf = [0u8; 16];
        let s = fmt_pair(&mut buf, cur as u32, total as u32);
        text(12, 124, "song:", DIM, 1);
        text(12 + 64, 120, s, ACCENT, 2);
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
