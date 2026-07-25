//! m4pocket player — VGM-плеер для Analogue Pocket.
//!
//! Читает VGM/VGZ из data-слота 1, конвертирует поток команд в слова
//! секвенсора chipbox (Wishbone, 0x8000_0000) и стримит их с
//! backpressure по заполнению FIFO. Тайминг исполняет железо.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::panic::PanicInfo;

use embedded_alloc::Heap;
use litex_openfpga::*;
use riscv_rt::entry;
use vgm_core::{decompress, Chip, Event, Gd3, Header, Reader};

mod files;
mod font;
mod ui;

/// Результат воспроизведения: что делать дальше в плейлисте
#[derive(Clone, Copy, PartialEq)]
enum Ctl {
    Next,
    Prev,
    /// Тот же трек с начала (после стопа)
    Restart,
    /// Прыжок на конкретный трек плейлиста (браузер)
    Jump(usize),
    /// Экран перерисовать, воспроизведение продолжается (выход из браузера)
    Redraw,
}

const BTN_UP: u16 = 1 << 0;
const BTN_DOWN: u16 = 1 << 1;
const BTN_A: u16 = 1 << 4;
const BTN_B: u16 = 1 << 5;
const BTN_R: u16 = 1 << 9;
const BTN_SEL: u16 = 1 << 14;
/// Бит паузы контрол-регистра chipbox (замораживает клоки чипов и тики)
const CTRL_PAUSE: u32 = 1 << 5;
/// Бит перемотки (тики секвенсора и play-тик в 8 раз быстрее)
const CTRL_FF: u32 = 1 << 6;

/// Контрол-регистр chipbox — единственный источник правды.
///
/// Регистр один и пишется целиком: любая запись переопределяет и режим
/// формата, и флаги транспорта. Раньше состояние перемотки дублировалось
/// в каждом экземпляре Buttons, и записи одного были не видны другому:
/// транспорт снимал R, а спин CmdSink считал, что менять нечего, — так
/// перемотка залипала на всю музыку. Теперь состояние здесь, в одном
/// месте, и слово всегда собирается из режима и актуальных флагов.
static CTRL_MODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static CTRL_FLAGS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn ctrl_commit() {
    use core::sync::atomic::Ordering::Relaxed;
    chipbox_write(2, CTRL_MODE.load(Relaxed) | CTRL_FLAGS.load(Relaxed));
}

/// Сменить режим формата (nsf/gbs/sid/cpu_run), сохранив флаги
fn ctrl_mode(m: u32) {
    CTRL_MODE.store(m, core::sync::atomic::Ordering::Relaxed);
    ctrl_commit();
}

fn ctrl_pause(on: bool) {
    ctrl_flag(CTRL_PAUSE, on, true);
}

/// Перемотка: зовётся из горячих циклов, поэтому пишет только при смене
fn ctrl_ff(on: bool) {
    ctrl_flag(CTRL_FF, on, false);
}

fn ctrl_flag(bit: u32, on: bool, always: bool) {
    use core::sync::atomic::Ordering::Relaxed;
    let cur = CTRL_FLAGS.load(Relaxed);
    let new = if on { cur | bit } else { cur & !bit };
    if new != cur || always {
        CTRL_FLAGS.store(new, Relaxed);
        ctrl_commit();
    }
}

/// Софт-сброс: чипы и FIFO в тишину, режим и флаги обнуляются
fn ctrl_reset() {
    use core::sync::atomic::Ordering::Relaxed;
    CTRL_MODE.store(0, Relaxed);
    CTRL_FLAGS.store(0, Relaxed);
    chipbox_write(2, 1);
}

/// Опрос кнопок по фронту с накоплением: scan() можно звать из любых
/// циклов ожидания (backpressure и пр.) — фронты копятся в pending и не
/// теряются, take() отдаёт накопленное. Иначе короткое нажатие в момент,
/// когда фирмварь ждёт FIFO, пропадало бесследно.
struct Buttons {
    last: u16,
    pending: u16,
}

impl Buttons {
    fn new() -> Buttons {
        Buttons { last: 0xFFFF, pending: 0 }
    }
    fn scan(&mut self) {
        let p = unsafe { litex_openfpga::litex_pac::Peripherals::steal() };
        let keys = p.APF_INPUT.cont1_key.read().bits() as u16;
        self.pending |= !self.last & keys;
        self.last = keys;
    }
    fn take(&mut self) -> u16 {
        self.scan();
        let e = self.pending;
        self.pending = 0;
        e
    }
    /// Перемотка уровнем R. Состояние — в общем CTRL_FLAGS, а не в
    /// экземпляре: два разных Buttons (транспорт и спин CmdSink) обязаны
    /// видеть записи друг друга, иначе отпускание R теряется.
    fn sync_ff(&mut self) {
        ctrl_ff(self.last & BTN_R != 0);
    }
}

/// Сообщение об ошибке + ожидание смены трека кнопками
fn error_wait(format: &str, msg: &str) -> Ctl {
    ui::screen(format, msg, "", "-", "-", None, None);
    let mut b = Buttons::new();
    loop {
        let e = b.take();
        if e & (BTN_RIGHT | BTN_DOWN) != 0 {
            return Ctl::Next;
        }
        if e & (BTN_LEFT | BTN_UP) != 0 {
            return Ctl::Prev;
        }
        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
    }
}

/// Система и чипы для UI по клокам VGM-заголовка
fn vgm_desc(c: &vgm_core::Clocks) -> (&'static str, String) {
    let mut chips = String::new();
    let mut add = |name: &str| {
        if !chips.is_empty() {
            chips.push('+');
        }
        chips.push_str(name);
    };
    if c.ym2612 != 0 { add("YM2612"); }
    if c.ym2151 != 0 { add("YM2151"); }
    if c.sn76489 != 0 { add("SN76489"); }
    if c.sega_pcm != 0 { add("SegaPCM"); }
    if c.okim6258 != 0 { add("MSM6258"); }
    if c.nes_apu != 0 { add("2A03"); }
    if c.ay8910 != 0 { add("AY/5B"); }
    if c.k051649 != 0 { add("SCC"); }
    if c.okim6295 != 0 { add("OKIM6295"); }
    if c.k053260 != 0 { add("K053260"); }
    if c.ymf262 != 0 { add("OPL3"); }
    if c.ym3812 != 0 { add("OPL2"); }
    if c.ym3526 != 0 { add("OPL"); }
    if c.gb_dmg != 0 { add("GB APU"); }
    let system = if c.ym2612 != 0 {
        "Sega Mega Drive"
    } else if c.ym2151 != 0 && c.sega_pcm != 0 {
        "Sega Arcade"
    } else if c.ym2151 != 0 && c.okim6258 != 0 {
        "Sharp X68000"
    } else if c.k051649 != 0 {
        "MSX"
    } else if c.ym3812 != 0 || c.ym3526 != 0 || c.ymf262 != 0 {
        "PC / AdLib"
    } else if c.nes_apu != 0 {
        "Famicom / NES"
    } else if c.sn76489 != 0 {
        "Sega Master System"
    } else if c.gb_dmg != 0 {
        "Game Boy"
    } else if c.ym2151 != 0 {
        "Arcade"
    } else {
        "VGM"
    };
    (system, chips)
}

#[global_allocator]
static HEAP: Heap = Heap::empty();

// Раскладка SDRAM (main_ram 0x4000_0000..0x4400_0000, 64 МБ):
//   0x4000_0000  программа + .bss (boot.bin)
//   0x40C0_0000  фреймбуфер litex
//   0x4100_0000  сырой файл из data-слота (до 8 МБ)
//   0x4180_0000  куча плеера (32 МБ — хватает на распакованный VGZ)
//   0x4380_0000+ стек (растёт вниз от 0x4400_0000)
const STAGE_BASE: u32 = 0x4100_0000;
const HEAP_BASE: usize = 0x4180_0000;
const HEAP_SIZE: usize = 32 * 1024 * 1024;


// chipbox на внешнем Wishbone-регионе LiteX
const CHIPBOX_BASE: *mut u32 = 0x8000_0000 as *mut u32;
const CHIPBOX_CLK_HZ: u64 = 57_120_000;

const OP_YM2151: u32 = 0x1000_0000;
const OP_AY: u32 = 0x2000_0000;
const OP_PCM: u32 = 0x3000_0000;
const OP_ADPCM: u32 = 0x4000_0000;
const OP_STR_ADDR: u32 = 0x5000_0000;
const OP_STR_START: u32 = 0x6000_0000;
const OP_STR_STOP: u32 = 0x7000_0000;
const OP_WAIT: u32 = 0x8000_0000;
const OP_APU: u32 = 0x9000_0000;
const OP_FM2612: u32 = 0xD000_0000;
const OP_SN: u32 = 0xE000_0000;
const OP_NESRAM_PTR: u32 = 0xA000_0000;
const OP_NESRAM_WR: u32 = 0xB000_0000;
/// OPL3: bank<<16 | reg<<8 | val (тот же опкод, что шлёт midi-core)
const OP_OPL3: u32 = 0xC000_0000;
// Расширенные чипы: опкод 0xF, суб-код в [27:24]. SCC = суб-код 0.
const OP_EXT: u32 = 0xF000_0000;
const EXT_SCC: u32 = 0x0000_0000;
const EXT_OKIM: u32 = 0x0100_0000;
const EXT_K060: u32 = 0x0200_0000;

/// База банка ADPCM-потоков в памяти сэмплов (PSRAM): нижние 4 МБ — ROM
/// SegaPCM, выше — данные DAC-стримов MSM6258.
const ADPCM_BASE: u32 = 0x40_0000;
/// NSF-данные в PSRAM (окно $8000-$FFFF через банки)
const NSF_PSRAM_BASE: u32 = 0x70_0000;
/// ROM сэмплов OKIM6295 в PSRAM (окно 0x100000-0x1FFFFF)
const OKIM_PSRAM_BASE: u32 = 0x10_0000;
/// ROM сэмплов K053260 в PSRAM (окно 0x200000-0x3FFFFF, до 2 МБ)
const K060_PSRAM_BASE: u32 = 0x20_0000;

fn chipbox_write(word_offset: usize, value: u32) {
    unsafe { CHIPBOX_BASE.add(word_offset).write_volatile(value) }
}

fn chipbox_status() -> u32 {
    unsafe { CHIPBOX_BASE.add(1).read_volatile() }
}

fn chipbox_read(word_offset: usize) -> u32 {
    unsafe { CHIPBOX_BASE.add(word_offset).read_volatile() }
}

/// Секунды с последнего софт-сброса (аппаратный tick_count @44100)
fn elapsed_s() -> u32 {
    chipbox_read(0x18) / 44_100
}

/// VU-метр ~12 Гц: чтение рега 0x1A отдаёт и очищает пики |L|/|R|
fn vu_tick(last: &mut u32) {
    let t = chipbox_read(0x18) / 3675;
    if t != *last {
        *last = t;
        let v = chipbox_read(0x1A);
        ui::vu(v as u16, (v >> 16) as u16);
    }
}

/// Диагностика GBS: t — play-тики, ДОСТАВЛЕННЫЕ в gb-домен, w — записи
/// SM83 в звуковые реги, f — фетчи из PSRAM. t=0 -> CDC тика мёртв;
/// t>0, w=0 -> PLAY не пишет в звук; всё растёт -> тракт вывода
fn diag_gb(buf: &mut [u8; 16]) -> &str {
    let tw = chipbox_read(0x1E);
    let f = chipbox_read(0x1D) >> 16;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf[0] = b't';
    for i in 0..2 {
        buf[1 + i] = HEX[(tw >> (24 - 4 * i) & 0xF) as usize];
    }
    buf[3] = b' ';
    buf[4] = b'w';
    for i in 0..2 {
        buf[5 + i] = HEX[(tw >> (8 - 4 * i) & 0xF) as usize];
    }
    buf[7] = b' ';
    buf[8] = b'f';
    for i in 0..4 {
        buf[9 + i] = HEX[(f >> (12 - 4 * i) & 0xF) as usize];
    }
    core::str::from_utf8(&buf[..13]).unwrap_or("?")
}

/// Пульс домена OPL (рег 0x1C, младшая половина)
fn diag_opl(buf: &mut [u8; 16]) -> &str {
    let v = chipbox_read(0x1C) & 0xFFFF;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf[0] = b'o';
    buf[1] = b':';
    for i in 0..4 {
        buf[2 + i] = HEX[(v >> (12 - 4 * i) & 0xF) as usize];
    }
    core::str::from_utf8(&buf[..6]).unwrap_or("?")
}

/// Диагностика CPU-форматов (рег 0x1B): p — обслуженные play-тики,
/// w — записи CPU в звуковые реги. p=0 -> CPU не крутит стаб;
/// p растёт, w=0 -> PLAY не пишет в чипы; оба растут -> тракт звука
fn diag_str(buf: &mut [u8; 16]) -> &str {
    let v = chipbox_read(0x1B);
    let f = chipbox_read(0x1D) & 0xFFFF;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf[0] = b'p';
    for i in 0..2 {
        buf[1 + i] = HEX[(v >> (24 - 4 * i) & 0xF) as usize];
    }
    buf[3] = b' ';
    buf[4] = b'w';
    for i in 0..2 {
        buf[5 + i] = HEX[(v >> (8 - 4 * i) & 0xF) as usize];
    }
    buf[7] = b' ';
    buf[8] = b'f';
    for i in 0..4 {
        buf[9 + i] = HEX[(f >> (12 - 4 * i) & 0xF) as usize];
    }
    core::str::from_utf8(&buf[..13]).unwrap_or("?")
}

/// Чтение байта PSRAM через отладочный канал 0x1F
fn psram_read(addr: u32) -> Option<u8> {
    chipbox_write(0x1F, addr);
    for _ in 0..10_000 {
        let v = chipbox_read(0x1F);
        if v & 0x100 != 0 {
            return Some(v as u8);
        }
        core::hint::spin_loop();
    }
    None
}

/// Заначка автовыбора слота в свободном окне PSRAM (переживает
/// перезапуск ядра — питание с карты не снимается)
const STASH_ADDR: u32 = 0x6F_0000;
const STASH_MAGIC: [u8; 4] = *b"M4S2";

fn stash_read() -> Option<(u32, [u32; 3])> {
    let mut b = [0u8; 17];
    for (i, sb) in b.iter_mut().enumerate() {
        *sb = psram_read(STASH_ADDR + i as u32)?;
    }
    if b[0..4] != STASH_MAGIC {
        return None;
    }
    let mut f = [0u32; 3];
    for (i, fv) in f.iter_mut().enumerate() {
        let o = 5 + i * 4;
        *fv = u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    }
    Some((b[4] as u32, f))
}

fn stash_write3(slot: u32, f: &[u32; 3]) {
    let mut b = [0u8; 18];
    b[0..4].copy_from_slice(&STASH_MAGIC);
    b[4] = slot as u8;
    for (i, fv) in f.iter().enumerate() {
        let o = 5 + i * 4;
        b[o..o + 4].copy_from_slice(&fv.to_le_bytes());
    }
    chipbox_write(8, STASH_ADDR);
    for pair in b.chunks(2) {
        chipbox_write(9, pair[0] as u32 | (pair[1] as u32) << 8);
    }
}

/// FNV-1a по буферу (отпечаток содержимого слота)
fn fnv1a(d: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in d {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    h
}

/// Проба слота по магии формата: getfile на железе часто молчит, но
/// чтение данных из слота работает — распознаём содержимое по сигнатуре
fn probe_slot(slot: u32) -> Option<&'static str> {
    unsafe { core::ptr::write_bytes(STAGE_BASE as *mut u8, 0, 16) };
    if !files::read_slot_to(slot, 512, STAGE_BASE) {
        return None;
    }
    let d = unsafe { core::slice::from_raw_parts(STAGE_BASE as *const u8, 16) };
    if &d[0..5] == b"NESM\x1a" {
        Some("NSF")
    } else if &d[0..3] == b"GBS" {
        Some("GBS")
    } else if &d[0..4] == b"PSID" || &d[0..4] == b"RSID" {
        Some("SID")
    } else if &d[0..4] == b"MThd" {
        Some("MIDI")
    } else if &d[0..4] == b"Vgm " || (d[0] == 0x1f && d[1] == 0x8b) {
        Some("VGM")
    } else if &d[0..4] == b"GYMX" {
        Some("GYM")
    } else if d[0] == b'#' || d.iter().all(|&b| (0x20..0x7F).contains(&b) || b == b'\r' || b == b'\n') {
        Some("M3U") // текстовый файл — почти наверняка плейлист
    } else {
        None
    }
}

/// Контекст плейлиста для браузера и счётчика треков
struct PlayCtx<'a> {
    list: &'a [String],
    idx: usize,
}

impl PlayCtx<'_> {
    fn track(&self) -> Option<(usize, usize)> {
        if self.list.len() > 1 {
            Some((self.idx, self.list.len()))
        } else {
            None
        }
    }
}

/// Имя трека для браузера: без каталогов и расширения
fn basename(p: &str) -> &str {
    let f = &p[p.rfind('/').map(|i| i + 1).unwrap_or(0)..];
    match f.rfind('.') {
        Some(d) if d > 0 => &f[..d],
        _ => f,
    }
}

/// Модальный браузер плейлиста (музыка продолжает играть из FIFO/железа).
/// Some(i) — прыгнуть на трек i, None — закрыть (нужна перерисовка).
fn browser(b: &mut Buttons, pl: &PlayCtx) -> Option<usize> {
    let mut cur = pl.idx;
    let draw = |cur: usize| {
        let mut names: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
        for p in pl.list {
            names.push(basename(p));
        }
        ui::browser(&names, cur, pl.idx, "playlist");
    };
    draw(cur);
    loop {
        let e = b.take();
        if e & BTN_DOWN != 0 {
            cur = if cur + 1 >= pl.list.len() { 0 } else { cur + 1 };
            draw(cur);
        } else if e & BTN_UP != 0 {
            cur = if cur == 0 { pl.list.len() - 1 } else { cur - 1 };
            draw(cur);
        } else if e & (BTN_RIGHT | BTN_A) != 0 {
            return Some(cur);
        } else if e & (BTN_LEFT | BTN_B | BTN_SEL) != 0 {
            return None;
        }
        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
    }
}

struct CmdSink {
    since_check: u32,
    /// Кнопки живут в синке: backpressure-спин может длиться секундами,
    /// и опрос внутри него — единственный способ не терять нажатия
    btn: Buttons,
    vu_last: u32,
}

impl CmdSink {
    fn new() -> CmdSink {
        CmdSink { since_check: 0, btn: Buttons::new(), vu_last: 0 }
    }

    /// Пуш команды с backpressure: держим FIFO не полнее ~1900 слов,
    /// статус читаем раз в 64 команды, чтобы не молотить шину.
    /// В спине живут VU и снятие перемотки (стримовые форматы: mode 0).
    fn push(&mut self, word: u32) {
        if self.since_check == 0 {
            // Опрос обязан идти НЕ только в спине backpressure. При
            // перемотке секвенсор дренит FIFO в 8 раз быстрее, спин почти
            // не запускается, а в плотных DAC-местах (ударные Mega Drive)
            // события Wait не приходят — отпускание R оставалось
            // незамеченным и перемотка залипала на всю музыку.
            self.btn.scan();
            self.btn.sync_ff();
            vu_tick(&mut self.vu_last);
            while chipbox_status() & 0x1FFF > 1900 {
                self.btn.scan();
                self.btn.sync_ff();
                vu_tick(&mut self.vu_last);
            }
            self.since_check = 64;
        }
        self.since_check -= 1;
        chipbox_write(0, word);
    }
}

/// Кнопки Pocket (cont1_key)
const BTN_LEFT: u16 = 1 << 2;
const BTN_RIGHT: u16 = 1 << 3;

/// Обработка кнопок во время воспроизведения: влево/вправо — треки
/// плейлиста, A — пауза, B — стоп. `mode` — биты контрол-регистра
/// текущего формата (для снятия паузы без потери режима).
/// Some(_) — покинуть цикл воспроизведения трека.
fn transport(b: &mut Buttons, mode: u32, pl: &PlayCtx) -> Option<Ctl> {
    let e = b.take();
    b.sync_ff(); // перемотка: пока R удержан (уровень, не фронт)
    if e & BTN_RIGHT != 0 {
        return Some(Ctl::Next);
    }
    if e & BTN_LEFT != 0 {
        return Some(Ctl::Prev);
    }
    if e & BTN_A != 0 {
        return hold(b, mode, false);
    }
    if e & BTN_B != 0 {
        return hold(b, mode, true);
    }
    if e & BTN_SEL != 0 && pl.list.len() > 1 {
        ctrl_pause(true); // тишина на время браузера
        let r = browser(b, pl);
        ctrl_pause(false);
        return match r {
            Some(i) => Some(Ctl::Jump(i)),
            None => Some(Ctl::Redraw),
        };
    }
    None
}

/// Пауза или стоп. Крутимся здесь: на паузе FIFO не дренится, стримить
/// нельзя. Из паузы A — продолжить, B — стоп; из стопа A/B — трек с
/// начала; влево/вправо работают всегда.
fn hold(b: &mut Buttons, mode: u32, mut stopped: bool) -> Option<Ctl> {
    loop {
        if stopped {
            ctrl_reset(); // сброс: чипы и FIFO в тишину
            ui::status("STOPPED");
        } else {
            ctrl_mode(mode); ctrl_pause(true);
            ui::status("PAUSED");
        }
        if stopped {
            // сброс не чистит канальные RAM (SegaPCM и пр.) — глушим микс
            chipbox_write(6, 0);
            chipbox_write(0xC, 0);
            chipbox_write(0x15, 0);
        }
        loop {
            let e = b.take();
            if e & BTN_RIGHT != 0 {
                ui::status("");
                return Some(Ctl::Next);
            }
            if e & BTN_LEFT != 0 {
                ui::status("");
                return Some(Ctl::Prev);
            }
            if e & BTN_A != 0 {
                ui::status("");
                if stopped {
                    return Some(Ctl::Restart);
                }
                ctrl_pause(false); // снять паузу
                return None;
            }
            if e & BTN_B != 0 {
                if stopped {
                    ui::status("");
                    return Some(Ctl::Restart);
                }
                stopped = true;
                break;
            }
            for _ in 0..20_000 {
                core::hint::spin_loop();
            }
        }
    }
}

/// Автопереход для форматов без известной длительности (NSF/GBS/SID)
const AUTO_NEXT_S: u32 = 180;

/// Общий цикл NSF/GBS/SID: вверх/вниз — подпесни, остальное — transport.
/// `start_song` полностью перезапускает воспроизведение (данные уже
/// в PSRAM, перегенерируется только стаб), `draw` рисует экран.
fn song_loop(
    num_songs: u8,
    start: u8,
    mode: u32,
    pl: &PlayCtx,
    lens: &[u32],
    gb_diag: bool,
    mut start_song: impl FnMut(u8),
    draw: impl Fn(u8),
) -> Ctl {
    let mut song = start;
    start_song(song);
    draw(song);
    let mut b = Buttons::new();
    let mut shown_s = u32::MAX;
    let mut vu_last = 0u32;
    loop {
        vu_tick(&mut vu_last);
        let edge = b.take();
        if edge & BTN_DOWN != 0 {
            song = if song + 1 >= num_songs { 0 } else { song + 1 };
            start_song(song);
            draw(song);
        } else if edge & BTN_UP != 0 {
            song = if song == 0 { num_songs.saturating_sub(1) } else { song - 1 };
            start_song(song);
            draw(song);
        } else if edge & BTN_RIGHT != 0 {
            return Ctl::Next;
        } else if edge & BTN_LEFT != 0 {
            return Ctl::Prev;
        } else if edge & (BTN_A | BTN_B) != 0 {
            match hold(&mut b, mode, edge & BTN_B != 0) {
                Some(Ctl::Restart) => {
                    start_song(song); // текущая подпесня заново
                    draw(song);
                }
                Some(ctl) => return ctl,
                None => {}
            }
        } else if edge & BTN_SEL != 0 && pl.list.len() > 1 {
            ctrl_mode(mode); ctrl_pause(true);
            let r = browser(&mut b, pl);
            ctrl_pause(false);
            match r {
                Some(i) => return Ctl::Jump(i),
                None => {
                    draw(song);
                    shown_s = u32::MAX;
                }
            }
        }
        b.sync_ff(); // перемотка уровнем R
        // время подпесни + автопереход: длительность из HVSC (SID) или
        // дефолт; после последней подпесни — следующий трек
        let limit = lens.get(song as usize).copied().unwrap_or(AUTO_NEXT_S).max(3);
        let el = elapsed_s();
        if el != shown_s {
            shown_s = el;
            let mut dbuf = [0u8; 16];
            let d = if gb_diag { diag_gb(&mut dbuf) } else { diag_str(&mut dbuf) };
            ui::progress(el.min(limit), limit, d);
        }
        if el >= limit {
            if song + 1 >= num_songs {
                return Ctl::Next;
            }
            song += 1;
            start_song(song);
            draw(song);
            shown_s = u32::MAX;
        }
        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
    }
}

/// Воспроизведение NSF: 6502 в chipbox исполняет INIT/PLAY, мы лишь
/// загружаем данные в PSRAM, собираем стаб и настраиваем тик.
fn nsf_play(data: &[u8], pl: &PlayCtx) -> Ctl {
    if data.len() < 0x80 {
        panic!("NSF слишком короткий");
    }
    let num_songs = data[0x06].max(1);
    let song = data[0x07].max(1) - 1; // 1-based в заголовке
    let load = u16::from_le_bytes([data[0x08], data[0x09]]);
    let init = u16::from_le_bytes([data[0x0A], data[0x0B]]);
    let play = u16::from_le_bytes([data[0x0C], data[0x0D]]);
    let period_us = u16::from_le_bytes([data[0x6E], data[0x6F]]);
    let banks: [u8; 8] = data[0x70..0x78].try_into().unwrap();
    let expansion = data[0x7B];
    let banked = banks.iter().any(|&b| b != 0);

    let name = core::str::from_utf8(&data[0x0E..0x2E]).unwrap_or("?");
    println!("NSF: {} (песня {})", name.trim_end_matches('\0'), song + 1);
    let has_5b = expansion & 0x20 != 0;
    let has_vrc6 = expansion & 0x01 != 0;
    if expansion & !(0x20 | 0x01) != 0 {
        println!("ВНИМАНИЕ: NSF просит expansion-чипы 0x{expansion:02x} — сыграет только поддержанное");
    }
    if load < 0x8000 {
        println!("NSF с load-адресом {load:#06x} < $8000 не поддержан");
        return error_wait("NSF", "load address < $8000 unsupported");
    }

    // Данные в PSRAM: при банкинге — с паддингом load&0xFFF (по спеке),
    // без — линейно от load-адреса (identity-банки по сбросу)
    let payload = &data[0x80..];
    let base_off = if banked { (load & 0x0FFF) as u32 } else { (load - 0x8000) as u32 };
    chipbox_write(8, NSF_PSRAM_BASE + base_off);
    for pair in payload.chunks(2) {
        let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
        chipbox_write(9, w);
    }

    // Клок NES и play-тик
    chipbox_write(0xB, ((1_789_773u64 << 32) / CHIPBOX_CLK_HZ) as u32);
    let period = if period_us == 0 { 16666.0 } else { period_us as f64 };
    let play_hz = 1_000_000.0 / period;
    chipbox_write(0xF, (play_hz / CHIPBOX_CLK_HZ as f64 * 4294967296.0) as u32);

    // APU в миксе; при 5B — ещё и AY на клоке NES
    if has_5b {
        chipbox_write(4, ((1_789_773u64 << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    chipbox_write(6, if has_5b { 64 << 8 } else { 0 });
    chipbox_write(0xC, 64);
    chipbox_write(0x15, 0);

    println!("NSF: {num_songs} песен, rate {play_hz:.1} Гц; D-pad влево/вправо — переключение");
    let artist = core::str::from_utf8(&data[0x2E..0x4E]).unwrap_or("");
    // VRC6 включается битом 7 контрол-регистра (декод $9xxx-$Bxxx в chipbox)
    let mode: u32 = 6 | if has_vrc6 { 0x80 } else { 0 };
    let chips = match (has_5b, has_vrc6) {
        (true, true) => "2A03+5B+VRC6",
        (true, false) => "2A03+5B",
        (false, true) => "2A03+VRC6",
        (false, false) => "2A03",
    };
    let draw = |s: u8| {
        ui::screen(
            "NSF",
            name.trim_end_matches('\0'),
            artist.trim_end_matches('\0'),
            "Famicom / NES",
            chips,
            Some((s + 1, num_songs)),
            pl.track(),
        );
    };

    song_loop(num_songs, song, mode, pl, &[], false, move |s| {
        // Стаб: SEI, банки (если надо), A=песня X=0(NTSC) Y=0, JSR INIT,
        // цикл по play-тику ($5FF0), JSR PLAY
        let mut stub: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        stub.push(0x78); // SEI
        if banked {
            for (i, &b) in banks.iter().enumerate() {
                stub.extend_from_slice(&[0xA9, b, 0x8D, 0xF8 + i as u8, 0x5F]);
            }
        }
        stub.extend_from_slice(&[0xA2, 0x00, 0xA0, 0x00]); // LDX #0, LDY #0
        stub.extend_from_slice(&[0xA9, s]); // LDA #песня
        stub.extend_from_slice(&[0x20, init as u8, (init >> 8) as u8]);
        let loop_at = stub.len() as u8;
        stub.extend_from_slice(&[0xAD, 0xF0, 0x5F]); // LDA $5FF0
        stub.extend_from_slice(&[0xF0, 0xFB]); // BEQ loop
        stub.extend_from_slice(&[0x8D, 0xF0, 0x5F]); // STA $5FF0
        stub.extend_from_slice(&[0x20, play as u8, (play >> 8) as u8]);
        stub.extend_from_slice(&[0x4C, loop_at, 0x50]); // JMP loop
        let rti_at = stub.len() as u8;
        stub.push(0x40); // RTI (NMI/IRQ)

        ctrl_mode(0); // остановить CPU
        for (i, &b) in stub.iter().enumerate() {
            chipbox_write(0xD, (i as u32) << 8 | b as u32);
        }
        // Векторы: NMI/IRQ -> RTI, RESET -> $5000
        let vecs = [rti_at, 0x50, 0x00, 0x50, rti_at, 0x50];
        for (i, &b) in vecs.iter().enumerate() {
            chipbox_write(0xE, (i as u32) << 8 | b as u32);
        }
        ctrl_reset(); // сброс чипов
        // гейны заново: стоп (hold) их глушит
        chipbox_write(6, if has_5b { 64 << 8 } else { 0 });
        chipbox_write(0xC, 64);
        chipbox_write(0x15, 0);
        ctrl_mode(mode); // nsf_mode | cpu_run (| vrc6_en)
        println!("NSF: песня {}", s + 1);
    }, draw)
}

/// Воспроизведение GBS: SM83 + GB APU в chipbox. Данные грузятся в PSRAM
/// линейно (банк N = смещение N*0x4000), стаб — DI, SP из заголовка,
/// A = песня, CALL INIT, цикл по play-тику ($FEA0), CALL PLAY.
fn gbs_play(data: &[u8], pl: &PlayCtx) -> Ctl {
    if data.len() < 0x70 {
        panic!("GBS слишком короткий");
    }
    let song = data[0x05].max(1) - 1;
    let load = u16::from_le_bytes([data[0x06], data[0x07]]);
    let init = u16::from_le_bytes([data[0x08], data[0x09]]);
    let play = u16::from_le_bytes([data[0x0A], data[0x0B]]);
    let sp = u16::from_le_bytes([data[0x0C], data[0x0D]]);
    let tma = data[0x0E];
    let tac = data[0x0F];

    let name = core::str::from_utf8(&data[0x10..0x30]).unwrap_or("?");
    println!("GBS: {} (песня {})", name.trim_end_matches('\0'), song + 1);
    if load < 0x400 {
        println!("GBS с load-адресом {load:#06x} < $0400 не поддержан (занято стабом)");
        return error_wait("GBS", "load address < $0400 unsupported");
    }

    // Данные линейно от load-адреса
    chipbox_write(8, NSF_PSRAM_BASE + load as u32);
    for pair in data[0x70..].chunks(2) {
        let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
        chipbox_write(9, w);
    }

    // Темп: таймер из заголовка или VBlank 59.73 Гц
    let play_hz = if tac & 0x04 != 0 {
        let base = match tac & 3 {
            0 => 4096.0,
            1 => 262144.0,
            2 => 65536.0,
            _ => 16384.0,
        };
        base / (256 - tma as u32) as f64
    } else {
        59.73
    };
    chipbox_write(0xF, (play_hz / CHIPBOX_CLK_HZ as f64 * 4294967296.0) as u32);

    // Только GB в миксе
    chipbox_write(6, 0);
    chipbox_write(0xC, 64 << 8);

    let num_songs = data[0x04].max(1);
    println!("GBS: {num_songs} песен, rate {play_hz:.2} Гц; D-pad влево/вправо — переключение");
    let author = core::str::from_utf8(&data[0x30..0x50]).unwrap_or("");
    let draw = |s: u8| {
        ui::screen(
            "GBS",
            name.trim_end_matches('\0'),
            author.trim_end_matches('\0'),
            "Game Boy",
            "SM83+GB APU",
            Some((s + 1, num_songs)),
            pl.track(),
        );
    };

    song_loop(num_songs, song, 0xC, pl, &[], true, move |s| {
        // Стаб: $00-$60 — RST/IRQ-трамплины JP LOAD+n (GBS-спека: драйверы
        // зовут RST как подпрограммы по LOAD+n — Metal Masters и др.);
        // тело на $00A0 (бут-инъекция железа приводит PC туда с $0000)
        let mut stub = alloc::vec![0u8; 0x100];
        let mut v = 0usize;
        while v <= 0x60 {
            let tgt = load.wrapping_add(v as u16);
            stub[v] = 0xC3;
            stub[v + 1] = tgt as u8;
            stub[v + 2] = (tgt >> 8) as u8;
            v += 8;
        }
        let mut o = 0xA0usize;
        stub[o] = 0xF3; // DI
        o += 1;
        stub[o..o + 3].copy_from_slice(&[0x31, sp as u8, (sp >> 8) as u8]);
        o += 3;
        stub[o..o + 2].copy_from_slice(&[0x3E, s]);
        o += 2;
        stub[o..o + 3].copy_from_slice(&[0xCD, init as u8, (init >> 8) as u8]);
        o += 3;
        let loop_at = o as u16;
        stub[o..o + 3].copy_from_slice(&[0xFA, 0xA0, 0xFE]);
        o += 3;
        stub[o] = 0xA7;
        o += 1;
        stub[o..o + 2].copy_from_slice(&[0x28, 0xFA]);
        o += 2;
        stub[o..o + 3].copy_from_slice(&[0xEA, 0xA0, 0xFE]);
        o += 3;
        stub[o..o + 3].copy_from_slice(&[0xCD, play as u8, (play >> 8) as u8]);
        o += 3;
        stub[o..o + 3].copy_from_slice(&[0xC3, loop_at as u8, (loop_at >> 8) as u8]);

        ctrl_mode(0); // остановить CPU
        for (i, &b) in stub.iter().enumerate() {
            chipbox_write(0x11, (i as u32) << 8 | b as u32);
        }
        ctrl_reset();
        // гейны заново: стоп (hold) их глушит
        chipbox_write(6, 0);
        chipbox_write(0xC, 64 << 8);
        chipbox_write(0x15, 0);
        ctrl_mode(0xC); // gbs_mode | cpu_run
        println!("GBS: песня {}", s + 1);
    }, draw)
}

/// Длительности подпесен SID из HVSC Songlengths.md5 (лежит рядом с
/// музыкой в Assets/pocketvgm/common). Формат: "<32hex>=M:SS M:SS ...".
/// Пусто — базы нет или записи не нашлось.
fn load_songlengths(md5h: &[u8; 32]) -> alloc::vec::Vec<u32> {
    let mut out = alloc::vec::Vec::new();
    if !files::open("Songlengths.md5") {
        return out;
    }
    let size = File::size(files::slot());
    if size == 0 || size == 0xFFFF_FFFF {
        return out;
    }
    let db = load_slot(size);
    let mut pos = 0usize;
    while pos + 33 <= db.len() {
        // строка вида md5=... — ищем совпадение в начале строки
        if db[pos..pos + 32] == md5h[..] && db[pos + 32] == b'=' {
            let mut i = pos + 33;
            let mut min = 0u32;
            let mut sec = 0u32;
            let mut in_sec = false;
            let mut skip = false; // хвост токена после секунд (.ms и пр.)
            while i < db.len() && db[i] != b'\n' {
                let c = db[i];
                match c {
                    b'0'..=b'9' if !skip => {
                        let v = if in_sec { &mut sec } else { &mut min };
                        *v = *v * 10 + (c - b'0') as u32;
                    }
                    b':' => in_sec = true,
                    b' ' => {
                        if in_sec {
                            out.push((min * 60 + sec).max(1));
                        }
                        min = 0;
                        sec = 0;
                        in_sec = false;
                        skip = false;
                    }
                    _ => skip = true,
                }
                i += 1;
            }
            if in_sec {
                out.push((min * 60 + sec).max(1));
            }
            break;
        }
        // к следующей строке
        while pos < db.len() && db[pos] != b'\n' {
            pos += 1;
        }
        pos += 1;
    }
    out
}

/// Воспроизведение PSID: 6502 в C64-карте памяти (вся 64К — PSRAM).
/// Заголовок big-endian. RSID и play=0 (свой IRQ-обработчик) не поддержаны.
fn sid_play(data: &[u8], pl: &PlayCtx) -> Ctl {
    let be16 = |o: usize| u16::from_be_bytes([data[o], data[o + 1]]);
    if data.len() < 0x76 {
        panic!("SID слишком короткий");
    }
    if &data[0..4] == b"RSID" {
        println!("RSID требует полной среды C64 — пока не поддержано");
        return error_wait("SID", "RSID unsupported (PSID only)");
    }
    let version = be16(0x04);
    let data_off = be16(0x06) as usize;
    let mut load = be16(0x08);
    let init = be16(0x0A);
    let play = be16(0x0C);
    let num_songs = (be16(0x0E).max(1) as u8).max(1);
    let start_song = (be16(0x10).max(1) - 1) as u8;
    let speed = u32::from_be_bytes([data[0x12], data[0x13], data[0x14], data[0x15]]);

    let name: String = core::str::from_utf8(&data[0x16..0x36])
        .unwrap_or("?").trim_end_matches('\0').into();
    println!("PSID v{version}: {name}");
    if play == 0 {
        println!("play-адрес 0 (свой IRQ-обработчик) — пока не поддержано");
        return error_wait("SID", "custom IRQ handler unsupported");
    }

    // PAL/NTSC и модель SID из флагов v2+
    let flags = if version >= 2 { be16(0x76) } else { 0 };
    let ntsc = flags & 0xC == 0x8;
    let v8580 = flags & 0x30 == 0x20;
    let sid_clk: u64 = if ntsc { 1_022_727 } else { 985_248 };
    chipbox_write(0x12, ((sid_clk << 32) / CHIPBOX_CLK_HZ) as u32);
    chipbox_write(0x13, v8580 as u32);

    // Данные: load=0 -> реальный адрес в первых двух байтах (LE)
    let mut body = &data[data_off..];
    if load == 0 {
        load = u16::from_le_bytes([body[0], body[1]]);
        body = &body[2..];
    }
    println!("load {load:#06x}, init {init:#06x}, play {play:#06x}, песен {num_songs}");

    // Только SID в миксе
    chipbox_write(6, 0);
    chipbox_write(0xC, 64 << 16);

    let vblank_hz = if ntsc { 59.83 } else { 50.12 };
    let body_vec: alloc::vec::Vec<u8> = body.into();
    let load_c = load;
    let author: String = core::str::from_utf8(&data[0x36..0x56])
        .unwrap_or("").trim_end_matches('\0').into();

    // Длительности подпесен из базы HVSC (Songlengths.md5 в папке музыки).
    // ВАЖНО: load_slot перезапишет staging — все нужные данные SID уже
    // скопированы выше (body_vec и owned-строки).
    let md5h = vgm_core::md5::md5_hex(data);
    let lens = load_songlengths(&md5h);
    if !lens.is_empty() {
        println!("HVSC: длительности найдены ({} подпесен)", lens.len());
    }
    let draw = |s: u8| {
        ui::screen(
            "SID",
            &name,
            &author,
            "Commodore 64",
            if v8580 { "SID 8580" } else { "SID 6581" },
            Some((s + 1, num_songs)),
            pl.track(),
        );
    };

    song_loop(num_songs, start_song, 0x14, pl, &lens, false, move |s| {
        ctrl_mode(0); // остановить CPU

        // чистый образ: нули + данные + стаб + векторы
        chipbox_write(8, NSF_PSRAM_BASE);
        for _ in 0..0x8000 {
            chipbox_write(9, 0);
        }
        chipbox_write(8, NSF_PSRAM_BASE + load_c as u32);
        for pair in body_vec.chunks(2) {
            let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
            chipbox_write(9, w);
        }

        let mut stub: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        stub.push(0x78); // SEI
        stub.extend_from_slice(&[0xA9, s]); // LDA #песня
        stub.extend_from_slice(&[0x20, init as u8, (init >> 8) as u8]);
        let loop_at = 0x0334 + stub.len() as u16;
        stub.extend_from_slice(&[0xAD, 0xF0, 0xD7]); // LDA $D7F0
        stub.extend_from_slice(&[0xF0, 0xFB]); // BEQ loop
        stub.extend_from_slice(&[0x8D, 0xF0, 0xD7]); // сброс тика
        stub.extend_from_slice(&[0x20, play as u8, (play >> 8) as u8]);
        stub.extend_from_slice(&[0x4C, loop_at as u8, (loop_at >> 8) as u8]);
        let rti_at = 0x0334 + stub.len() as u16;
        stub.push(0x40); // RTI
        chipbox_write(8, NSF_PSRAM_BASE + 0x334);
        for pair in stub.chunks(2) {
            let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
            chipbox_write(9, w);
        }
        let vecs = [
            rti_at as u8, (rti_at >> 8) as u8,
            0x34, 0x03,
            rti_at as u8, (rti_at >> 8) as u8,
        ];
        chipbox_write(8, NSF_PSRAM_BASE + 0xFFFA);
        for pair in vecs.chunks(2) {
            chipbox_write(9, pair[0] as u32 | (pair[1] as u32) << 8);
        }

        // темп песни: бит в speed-маске: 1 = CIA ~60 Гц, 0 = VBlank
        let bit = speed >> core::cmp::min(s as u32, 31) & 1;
        let hz = if bit != 0 { 60.0 } else { vblank_hz };
        chipbox_write(0xF, (hz / CHIPBOX_CLK_HZ as f64 * 4294967296.0) as u32);

        ctrl_reset(); // сброс чипов
        // гейны заново: стоп (hold) их глушит
        chipbox_write(6, 0);
        chipbox_write(0xC, 64 << 16);
        chipbox_write(0x15, 0);
        ctrl_mode(0x14); // sid_mode | cpu_run
        println!("SID: песня {} ({hz:.1} Гц)", s + 1);
    }, draw)
}

/// MIDI: конвертация в поток OPL3-команд (midi-core) и стриминг в FIFO,
/// как VGM. Два прохода — и дальше по плейлисту.
fn midi_play(data: &[u8], pl: &PlayCtx) -> Ctl {
    println!("MIDI: конвертирую (GM на OPL3)...");
    let cmds = match midi_core::midi_to_commands(data) {
        Ok(c) => c,
        Err(e) => {
            // не паникуем: показать ошибку и дать листать дальше
            let mut msg = String::from("MIDI err: ");
            msg.push_str(match e {
                midi_core::Error::BadMagic => "bad magic",
                midi_core::Error::TooShort => "too short",
                midi_core::Error::BadTrack => "bad track",
                midi_core::Error::NoGenmidi => "no genmidi",
            });
            return error_wait("MIDI", &msg);
        }
    };
    println!("MIDI: {} команд, играю", cmds.len());
    let draw = || ui::screen("MIDI", "", "", "PC / General MIDI", "OPL3 FM", None, pl.track());
    draw();
    // длительность одного прохода — сумма WAIT'ов
    let pass_ticks: u32 = cmds.iter()
        .filter(|&&c| c & 0xF000_0000 == 0x8000_0000)
        .map(|&c| c & 0xFF_FFFF)
        .sum();
    let total_s = pass_ticks / 44_100 * 2;

    chipbox_write(6, 0);
    chipbox_write(0xC, 64 << 24); // только OPL3
    ctrl_reset();

    let mut sink = CmdSink::new();
    let mut shown_s = u32::MAX;
    let mut vu_last = 0u32;
    for _pass in 0..2 {
        for chunk in cmds.chunks(256) {
            for &c in chunk {
                sink.push(c);
            }
            match transport(&mut sink.btn, 0, pl) {
                Some(Ctl::Redraw) => draw(),
                Some(ctl) => return ctl,
                None => {}
            }
            vu_tick(&mut vu_last);
            let el = elapsed_s();
            if el != shown_s {
                shown_s = el;
                let mut dbuf = [0u8; 16];
                ui::progress(el.min(total_s), total_s, diag_opl(&mut dbuf));
            }
        }
    }
    while chipbox_status() & 0x1FFF != 0 {
        match transport(&mut sink.btn, 0, pl) {
            Some(Ctl::Redraw) => draw(),
            Some(ctl) => return ctl,
            None => {}
        }
        core::hint::spin_loop();
    }
    Ctl::Next
}

/// Воспроизведение GYM (лог Genesis: YM2612+PSG, кадры 1/60 с).
/// GYMX-заголовок 428 байт: магия, название/игра, loop-кадр, zlib-флаг.
fn gym_play(staged: &'static [u8], pl: &PlayCtx) -> Ctl {
    let mut title = String::new();
    let mut sub = String::new();
    let mut loop_frame: u32 = 0;
    let unpacked;
    let body: &[u8] = if staged.len() >= 428 && &staged[0..4] == b"GYMX" {
        let cstr = |o: usize, n: usize| -> String {
            let raw = &staged[o..o + n];
            let len = raw.iter().position(|&b| b == 0).unwrap_or(n);
            String::from_utf8_lossy(&raw[..len]).into_owned()
        };
        title = cstr(4, 32);
        sub = cstr(36, 32);
        loop_frame = u32::from_le_bytes([staged[420], staged[421], staged[422], staged[423]]);
        let packed = u32::from_le_bytes([staged[424], staged[425], staged[426], staged[427]]);
        if packed != 0 {
            match vgm_core::decompress_zlib(&staged[428..]) {
                Some(v) => {
                    unpacked = v;
                    &unpacked
                }
                None => return error_wait("GYM", "zlib decompress error"),
            }
        } else {
            &staged[428..]
        }
    } else {
        staged // безголовый GYM: сразу поток команд
    };

    // прескан: число кадров и байтовое смещение loop-кадра
    let mut frames: u32 = 0;
    let mut loop_off: usize = 0;
    {
        let mut i = 0usize;
        while i < body.len() {
            match body[i] {
                0x00 => {
                    frames += 1;
                    if loop_frame != 0 && frames == loop_frame {
                        loop_off = i + 1;
                    }
                    i += 1;
                }
                0x01 | 0x02 => i += 3,
                0x03 => i += 2,
                _ => break, // мусор/конец
            }
        }
    }
    let total_s = (frames + frames.saturating_sub(loop_frame)) / 60;

    ui::screen(
        "GYM",
        &title,
        &sub,
        "Sega Mega Drive",
        "YM2612+SN76489",
        None,
        pl.track(),
    );
    let draw = || {
        ui::screen("GYM", &title, &sub, "Sega Mega Drive", "YM2612+SN76489", None, pl.track());
    };

    // клоки/гейны Genesis (как VGM)
    chipbox_write(0x16, ((7_670_453u64 << 32) / CHIPBOX_CLK_HZ) as u32);
    chipbox_write(0x17, ((3_579_545u64 << 32) / CHIPBOX_CLK_HZ) as u32);
    chipbox_write(0x15, 32u32 << 8 | 64);
    chipbox_write(6, 0);
    chipbox_write(0xC, 0);
    ctrl_reset();

    let mut sink = CmdSink::new();
    let mut shown_s = u32::MAX;
    for pass in 0..2u32 {
        let mut i = if pass == 0 { 0 } else { loop_off };
        while i < body.len() {
            match body[i] {
                0x00 => {
                    sink.push(OP_WAIT | 735);
                    i += 1;
                    match transport(&mut sink.btn, 0, pl) {
                        Some(Ctl::Redraw) => {
                            draw();
                            shown_s = u32::MAX;
                        }
                        Some(ctl) => return ctl,
                        None => {}
                    }
                    let el = elapsed_s();
                    if el != shown_s {
                        shown_s = el;
                        ui::progress(el.min(total_s), total_s, "");
                    }
                }
                0x01 | 0x02 => {
                    if i + 2 >= body.len() {
                        break;
                    }
                    let port = (body[i] - 1) as u32;
                    sink.push(OP_FM2612 | port << 16 | (body[i + 1] as u32) << 8 | body[i + 2] as u32);
                    i += 3;
                }
                0x03 => {
                    if i + 1 >= body.len() {
                        break;
                    }
                    sink.push(OP_SN | body[i + 1] as u32);
                    i += 2;
                }
                _ => break,
            }
        }
        if loop_frame == 0 && pass == 0 {
            // без луп-точки: второй проход с начала
        }
    }
    while chipbox_status() & 0x1FFF != 0 {
        match transport(&mut sink.btn, 0, pl) {
            Some(Ctl::Redraw) => draw(),
            Some(ctl) => return ctl,
            None => {}
        }
        core::hint::spin_loop();
    }
    Ctl::Next
}

/// Строка GD3 по индексу (0 трек, 2 игра, 6 автор), пустая -> None
fn gd3_field(gd3: &Gd3, n: usize) -> Option<String> {
    let mut s = String::new();
    for unit in char::decode_utf16(gd3.string(n)) {
        s.push(unit.unwrap_or('?'));
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// (название, «игра - автор») из GD3
fn gd3_lines(data: &[u8], header: &Header) -> (Option<String>, String) {
    let gd3 = match header.gd3_offset.and_then(|o| Gd3::parse(data, o)) {
        Some(g) => g,
        None => return (None, String::new()),
    };
    let title = gd3_field(&gd3, 0);
    let mut sub = gd3_field(&gd3, 2).unwrap_or_default();
    if let Some(author) = gd3_field(&gd3, 6) {
        if !sub.is_empty() {
            sub.push_str(" - ");
        }
        sub.push_str(&author);
    }
    (title, sub)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("Паника: {info}");
    let msg = alloc::format!("{info}");
    let cut = &msg[..msg.len().min(60)];
    ui::screen("PANIC", cut, "", "-", "-", None, None);
    loop {}
}

#[entry]
fn main() -> ! {
    unsafe { HEAP.init(HEAP_BASE, HEAP_SIZE) };

    println!("m4pocket player v0.1");
    ui::init();
    ui::screen("loading...", "", "", "-", "-", None, None);

    // Расширения разнесены по двум слотам (лимит APF — 4 на слот).
    // APF не сообщает, какой слот обновился последним: если файлы есть
    // в обоих — честно спрашиваем пользователя на старте.
    // размер невыбранного слота — мусор из datatable, надёжен только
    // путь из getfile: пустой путь = слот не выбирался
    // Три группированных слота (Sega/Nintendo/Computer). Меню выбора НЕТ:
    // (1) главный сигнал — id последнего dataslot_update от Pocket
    //     (WB 0x20) — это и есть «какой Load нажали»;
    // (2) резерв — отпечатки содержимого слотов против заначки в PSRAM
    //     (изменившийся слот = выбранный);
    // (3) дальше — последний игранный слот из заначки, иначе первый
    //     непустой.
    let mut present = [false; 3];
    let mut fps = [0u32; 3];
    for i in 0..3usize {
        let slot = i as u32 + 1;
        files::set_slot(slot);
        let has = !files::slot_path().is_empty() || probe_slot(slot).is_some();
        present[i] = has;
        if has && files::read_slot_to(slot, 4096, STAGE_BASE) {
            fps[i] = fnv1a(unsafe { core::slice::from_raw_parts(STAGE_BASE as *const u8, 4096) });
        }
    }
    let n_present = present.iter().filter(|&&p| p).count();
    if n_present == 0 {
        println!("Нет файла в слотах Music");
        ui::screen("no file", "select music in core settings", "", "-", "-", None, None);
        loop {}
    }

    let upd = chipbox_read(0x20);
    let upd_cnt = (upd >> 16) & 0xFF;
    let upd_id = upd & 0xFFFF;
    let stash = stash_read();

    let mut chosen: u32 = 0;
    // Явный выбор = Pocket сообщил про обновление слота. Только тогда
    // играем сразу; иначе показываем титул, чтобы ядро не начинало молча
    // проигрывать прошлое или случайное.
    let mut picked_by_user = false;
    // (1) Pocket сообщил обновлённый слот — верим ему, если слот непуст
    if upd_cnt > 0 && (1..=3).contains(&upd_id) && present[(upd_id - 1) as usize] {
        chosen = upd_id;
        picked_by_user = true;
    }
    // (2) изменившийся отпечаток
    if chosen == 0 {
        if let Some((last, sf)) = stash {
            let changed: alloc::vec::Vec<u32> = (0..3)
                .filter(|&i| present[i] && fps[i] != sf[i])
                .map(|i| i as u32 + 1)
                .collect();
            if changed.len() == 1 {
                chosen = changed[0];
            } else if changed.is_empty() && (1..=3).contains(&last) && present[(last - 1) as usize] {
                chosen = last; // ничего не менялось — прошлый слот
            }
        }
    }
    // (3) первый непустой
    if chosen == 0 {
        chosen = present.iter().position(|&p| p).unwrap() as u32 + 1;
    }
    files::set_slot(chosen);
    stash_write3(chosen, &fps);

    // Титул: ядро не проигрывает ничего само. Экран информационный —
    // трек здесь не предлагается и не ищется, музыка включается только
    // через Load, который перезапускает ядро с выбранным файлом.
    if !picked_by_user {
        ui::title(concat!("v", env!("CARGO_PKG_VERSION")));
        let mut b = Buttons::new();
        loop {
            if b.take() & (BTN_A | BTN_SEL) != 0 {
                break;
            }
            for _ in 0..20_000 {
                core::hint::spin_loop();
            }
        }
    }

    let size = File::size(files::slot());

    // Плейлист: выбранный .m3u; иначе playlist.m3u рядом с файлом;
    // иначе одиночный трек
    let own_path = files::slot_path();
    println!("Файл: {own_path}");
    let staged = load_slot(size);
    let base = String::from(files::dir_of(&own_path));

    let mut list: alloc::vec::Vec<String>;
    let mut idx: usize = 0;

    let is_m3u = own_path.len() > 4
        && own_path[own_path.len() - 4..].eq_ignore_ascii_case(".m3u");
    if is_m3u {
        list = files::parse_m3u(staged, &base);
        println!("Плейлист: {} треков", list.len());
        if list.is_empty() {
            ui::screen("m3u", "playlist empty or unreadable", "", "-", "-", None, None);
            loop {}
        }
    } else {
        // Плейлист рядом с треком: playlist.m3u, затем «Имя папки.m3u»
        // (vgmrips кладёт плейлист с именем альбома) и вариант _ -> пробел
        let dirname = {
            let d = base.trim_end_matches('/');
            &d[d.rfind('/').map(|i| i + 1).unwrap_or(0)..]
        };
        let mut cands: alloc::vec::Vec<String> = alloc::vec::Vec::new();
        let mut push_cand = |name: &str| {
            if !name.is_empty() {
                let mut p = String::from(base.as_str());
                p.push_str(name);
                p.push_str(".m3u");
                cands.push(p);
            }
        };
        push_cand("playlist");
        push_cand(dirname);
        if dirname.contains('_') {
            let spaced: String = dirname.chars().map(|c| if c == '_' { ' ' } else { c }).collect();
            push_cand(&spaced);
        }
        cands.dedup();

        list = alloc::vec::Vec::new();
        for cand in &cands {
            if files::open(cand) {
                let psize = File::size(files::slot());
                if psize == 0 || psize > 0x40_0000 {
                    continue; // не похоже на плейлист
                }
                let pdata = load_slot(psize);
                list = files::parse_m3u(pdata, &base);
                idx = list.iter().position(|p| *p == own_path).unwrap_or(0);
                println!("Найден плейлист {cand}: {} треков", list.len());
                break;
            }
        }
        if list.is_empty() {
            list.push(own_path.clone());
        }
    }

    // какой файл сейчас реально открыт в слоте: выбранный из меню уже
    // там — переоткрывать его через openfile не нужно (и нельзя
    // зависеть от openfile для базового воспроизведения)
    let mut in_slot: String = own_path.clone();

    loop {
        let path = list[idx].clone();
        println!("Трек {}/{}: {path}", idx + 1, list.len());
        let pl = PlayCtx { list: &list, idx };

        // Глушим чипы ДО открытия и чтения файла: и то, и другое заметно
        // блокирует (APF-запрос + перекачка в staging), а до этого фикса
        // сброс делался только внутри воспроизведения — всё это время
        // предыдущий трек продолжал тянуть последнюю ноту.
        ctrl_reset(); // софт-сброс: чистит FIFO и глушит чипы

        let opened = if path == in_slot {
            true
        } else if files::open(&path) {
            in_slot = path.clone();
            true
        } else {
            false
        };
        let ctl = if !opened {
            // код ошибки APF и хвост пути — для диагностики с экрана
            let tail = &path[path.len().saturating_sub(26)..];
            let mut msg = String::from("open err ");
            msg.push((b'0' + (files::last_err() % 8) as u8) as char);
            msg.push_str(": ");
            msg.push_str(tail);
            error_wait("error", &msg)
        } else {
            let fsize = File::size(files::slot());
            if fsize == 0 || fsize == 0xFFFF_FFFF {
                error_wait("error", "empty file")
            } else {
                let data = load_slot(fsize);
                dispatch(data, &pl)
            }
        };

        match ctl {
            Ctl::Next => idx = if idx + 1 >= list.len() { 0 } else { idx + 1 },
            Ctl::Prev => idx = if idx == 0 { list.len() - 1 } else { idx - 1 },
            Ctl::Jump(i) => idx = i.min(list.len() - 1),
            Ctl::Restart | Ctl::Redraw => {} // тот же трек с начала
        }
    }
}

/// Чтение содержимого слота в staging-буфер
fn load_slot(size: u32) -> &'static [u8] {
    File::request_read(0, size, STAGE_BASE, files::slot());
    File::block_op_complete();
    unsafe { core::slice::from_raw_parts(STAGE_BASE as *const u8, size as usize) }
}

/// Определение формата по магии и запуск
fn dispatch(staged: &'static [u8], pl: &PlayCtx) -> Ctl {
    if staged.len() >= 5 && &staged[0..5] == b"NESM\x1a" {
        return nsf_play(staged, pl);
    }
    if staged.len() >= 4 && &staged[0..3] == b"GBS" && staged[3] == 1 {
        return gbs_play(staged, pl);
    }
    if staged.len() >= 4 && (&staged[0..4] == b"PSID" || &staged[0..4] == b"RSID") {
        return sid_play(staged, pl);
    }
    if staged.len() >= 4 && &staged[0..4] == b"MThd" {
        return midi_play(staged, pl);
    }
    let path = &pl.list[pl.idx];
    let low: String = path.chars().map(|c| c.to_ascii_lowercase()).collect();
    if (staged.len() >= 4 && &staged[0..4] == b"GYMX") || low.ends_with(".gym") {
        return gym_play(staged, pl);
    }
    vgm_play(staged, pl)
}

/// Воспроизведение VGM/VGZ: два прохода лупа, затем следующий трек
fn vgm_play(staged: &'static [u8], pl: &PlayCtx) -> Ctl {
    // .vgz распаковываем в кучу; сырой .vgm играем прямо из staging-буфера
    let decompressed;
    let data: &[u8] = if staged.len() >= 2 && staged[0..2] == vgm_core::GZIP_MAGIC {
        match decompress(staged) {
            Ok(v) => {
                decompressed = v;
                &decompressed
            }
            Err(_) => return error_wait("VGM", "vgz decompress error"),
        }
    } else {
        staged
    };

    let header = match Header::parse(data) {
        Ok(h) => h,
        Err(_) => {
            let path = &pl.list[pl.idx];
            let tail = &path[path.len().saturating_sub(24)..];
            let mut msg = String::from("unknown format: ");
            msg.push_str(tail);
            return error_wait("?", &msg);
        }
    };

    let (title, sub) = gd3_lines(data, &header);
    let (system, chips) = vgm_desc(&header.clocks);
    let draw = || {
        ui::screen("VGM", title.as_deref().unwrap_or(""), &sub, system, &chips, None, pl.track());
    };
    draw();
    if let Some(t) = &title {
        println!("Трек: {t}");
    }
    // длительность двух проходов: полный файл + ещё один луп
    let total_s = (header.total_ticks.saturating_add(header.loop_ticks)) / 44_100;

    let ym_clk = header.clocks.ym2151;
    let ay_clk = header.clocks.ay8910;
    let pcm_clk = header.clocks.sega_pcm;
    let adpcm_clk = header.clocks.okim6258;
    if ym_clk == 0
        && ay_clk == 0
        && pcm_clk == 0
        && adpcm_clk == 0
        && header.clocks.nes_apu == 0
        && header.clocks.ym2612 == 0
        && header.clocks.sn76489 == 0
        && header.clocks.k051649 == 0
        && header.clocks.okim6295 == 0
        && header.clocks.k053260 == 0
        && header.clocks.ym3812 == 0
        && header.clocks.ym3526 == 0
        && header.clocks.ymf262 == 0
    {
        println!("В этом VGM нет поддержанных чипов");
        return error_wait("VGM", "no supported chips in this file");
    }
    println!(
        "YM2151 @ {ym_clk} Гц, AY @ {ay_clk} Гц, SegaPCM @ {pcm_clk} Гц, MSM6258 @ {adpcm_clk} Гц, играю (v{:x})",
        header.version
    );

    // Частоты чипов, баланс и сброс
    if ym_clk != 0 {
        chipbox_write(3, (((ym_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    if ay_clk != 0 {
        chipbox_write(4, (((ay_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    if pcm_clk != 0 {
        chipbox_write(5, (((pcm_clk as u64 * 2) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    if adpcm_clk != 0 {
        chipbox_write(7, (((adpcm_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
        chipbox_write(0xA, (header.clocks.okim6258_flags & 3) as u32);
    }
    let nes_clk = header.clocks.nes_apu;
    if nes_clk != 0 {
        chipbox_write(0xB, (((nes_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    let fm_clk = header.clocks.ym2612;
    let sn_clk = header.clocks.sn76489;
    if fm_clk != 0 {
        chipbox_write(0x16, (((fm_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    if sn_clk != 0 {
        chipbox_write(0x17, (((sn_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    let scc_clk = header.clocks.k051649;
    if scc_clk != 0 {
        chipbox_write(0x21, (((scc_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
        chipbox_write(0x22, 64); // scc_gain
    }
    // OPL-семейство (YM3812/YM3526/YMF262) играет на нашем OPL3. Клок OPL3
    // номинально 14.32 МГц, но ядро тактуется master-клоком x2 (25.45 МГц
    // по умолчанию); OPL2-файлы задают 3.58 МГц — пересчитываем в x4.
    let opl_clk = if header.clocks.ymf262 != 0 {
        header.clocks.ymf262
    } else if header.clocks.ym3812 != 0 {
        header.clocks.ym3812 * 4
    } else if header.clocks.ym3526 != 0 {
        header.clocks.ym3526 * 4
    } else {
        0
    };
    if opl_clk != 0 {
        chipbox_write(0x14, (((opl_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    let okim_clk = header.clocks.okim6295 & 0x7FFF_FFFF; // бит31 = флаг чипа
    if okim_clk != 0 {
        chipbox_write(0x23, (((okim_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
        chipbox_write(0x24, 1 << 8 | 64); // ss=1, okim_gain=64
    }
    let k060_clk = header.clocks.k053260;
    if k060_clk != 0 {
        chipbox_write(0x25, (((k060_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
        chipbox_write(0x26, 64); // k060_gain
    }
    // Genesis-баланс: PSG заметно тише FM
    chipbox_write(
        0x15,
        if sn_clk != 0 { 32u32 } else { 0 } << 8 | if fm_clk != 0 { 64u32 } else { 0 },
    );
    // Гейны: неиспользуемые чипы глушим; SegaPCM 34/64 — баланс Out Run
    // по MAME (0.30 FM / 0.70 PCM с учётом нативных амплитуд ядер)
    let gains = if adpcm_clk != 0 { 64u32 } else { 0 } << 24
        | if pcm_clk != 0 { 34u32 } else { 0 } << 16
        | if ay_clk != 0 { 64u32 } else { 0 } << 8
        | if ym_clk != 0 { 64u32 } else { 0 };
    chipbox_write(6, gains);
    // {opl_gain, sid_gain, gb_gain, apu_gain}. У VGM-AdLib регистры громкости
    // выкручены сильнее, чем у нашего MIDI-конвертера: на 64 выход клиппит
    // (проверено на Dune в симуляции), поэтому для VGM берём 16.
    chipbox_write(
        0xC,
        if opl_clk != 0 { 16u32 } else { 0 } << 24 | if nes_clk != 0 { 64u32 } else { 0 },
    );
    ctrl_reset();

    let mut sink = CmdSink::new();
    // SCC: разблокировать регистры звука (BR2=0x3F) первой командой FIFO
    if scc_clk != 0 {
        sink.push(OP_EXT | EXT_SCC | (7u32 << 16));
    }
    let mut reader = Reader::new(data, header.data_offset);
    let mut loops: u32 = 0;
    let mut shown_s = u32::MAX;
    let mut vu_last = 0u32;

    // Банк DAC-стримов MSM6258: блоки типа 0x04 конкатенируются в PSRAM
    // по ADPCM_BASE; их границы нужны для команды 0x95
    let mut adpcm_blocks: alloc::vec::Vec<(u32, u32)> = alloc::vec::Vec::new();
    let mut adpcm_bank_size: u32 = 0;
    // Банк DAC-сэмплов YM2612 (data-блоки 0x00), читается фирмварью
    let mut dac_bank: alloc::vec::Vec<u8> = alloc::vec::Vec::new();

    loop {
        match reader.next_event() {
            Ok(Event::Write { chip: Chip::Ym2151, addr, data, .. }) => {
                sink.push(OP_YM2151 | (addr as u32) << 8 | data as u32);
            }
            Ok(Event::Write { chip: Chip::Ym2612, port, addr, data }) => {
                sink.push(OP_FM2612 | (port as u32) << 16 | (addr as u32) << 8 | data as u32);
            }
            Ok(Event::Write { chip: Chip::Sn76489, data, .. }) => {
                sink.push(OP_SN | data as u32);
            }
            Ok(Event::Write { chip: Chip::Opl, port, addr, data }) => {
                // OPL2/OPL3 играем на нашем OPL3: port = банк регистров
                sink.push(OP_OPL3 | (port as u32) << 16 | (addr as u32) << 8 | data as u32);
            }
            Ok(Event::Write { chip: Chip::K051649, port, addr, data }) => {
                sink.push(OP_EXT | EXT_SCC | (port as u32) << 16 | (addr as u32) << 8 | data as u32);
            }
            Ok(Event::Ym2612Dac { ticks, offset }) => {
                let b = *dac_bank.get(offset as usize).unwrap_or(&0);
                sink.push(OP_FM2612 | 0x2A << 8 | b as u32);
                if ticks > 0 {
                    sink.push(OP_WAIT | ticks as u32);
                }
            }
            Ok(Event::DataBlock { kind: 0x00, start, len }) => {
                dac_bank.extend_from_slice(&data[start..start + len]);
            }
            Ok(Event::Write { chip: Chip::Ay8910, addr, data, .. }) => {
                sink.push(OP_AY | ((addr & 0xF) as u32) << 8 | data as u32);
            }
            Ok(Event::SegaPcmWrite { offset, data }) => {
                sink.push(OP_PCM | ((offset & 0xFF) as u32) << 8 | data as u32);
            }
            Ok(Event::DataBlock { kind: 0x80, start, len }) if len >= 8 => {
                // ROM-образ SegaPCM: [размер u32][смещение u32][данные]
                let block = &data[start..start + len];
                let rom_off = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
                let bytes = &block[8..];
                println!("SegaPCM ROM: {} байт @ 0x{rom_off:x}", bytes.len());
                chipbox_write(8, rom_off);
                for pair in bytes.chunks(2) {
                    let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
                    chipbox_write(9, w);
                }
            }
            Ok(Event::DataBlock { kind: 0x04, start, len }) => {
                // Данные DAC-стрима MSM6258 → в банк ADPCM в PSRAM
                let bytes = &data[start..start + len];
                adpcm_blocks.push((adpcm_bank_size, len as u32));
                chipbox_write(8, ADPCM_BASE + adpcm_bank_size);
                for pair in bytes.chunks(2) {
                    let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
                    chipbox_write(9, w);
                }
                adpcm_bank_size += len as u32;
            }
            Ok(Event::DacStream { cmd, start, len }) => {
                let p = &data[start..start + len];
                match cmd {
                    0x93 => {
                        let a = u32::from_le_bytes([p[1], p[2], p[3], p[4]]);
                        let ll = u32::from_le_bytes([p[6], p[7], p[8], p[9]]);
                        let n = match p[5] {
                            1 => ll,
                            3 => adpcm_bank_size.saturating_sub(a),
                            _ => 0,
                        };
                        sink.push(OP_STR_ADDR | (ADPCM_BASE + a) & 0xFF_FFFF);
                        if n != 0 {
                            sink.push(OP_STR_START | n & 0xFF_FFFF);
                        }
                    }
                    0x94 => sink.push(OP_STR_STOP),
                    0x95 => {
                        let blk = p[1] as usize | (p[2] as usize) << 8;
                        if let Some(&(off, n)) = adpcm_blocks.get(blk) {
                            sink.push(OP_STR_ADDR | (ADPCM_BASE + off) & 0xFF_FFFF);
                            sink.push(OP_STR_START | n & 0xFF_FFFF);
                        }
                    }
                    _ => {} // 0x90..0x92 — настройка стрима, нам не нужна
                }
            }
            Ok(Event::Write { chip: Chip::Okim6258, addr, data: d, .. }) => {
                sink.push(OP_ADPCM | ((addr & 3) as u32) << 8 | d as u32);
            }
            Ok(Event::Write { chip: Chip::Okim6295, data: d, .. }) => {
                sink.push(OP_EXT | EXT_OKIM | d as u32);
            }
            Ok(Event::DataBlock { kind: 0x8B, start, len }) if len >= 8 => {
                // ROM-образ OKIM6295: [u32 полный размер][u32 смещение][данные]
                let block = &data[start..start + len];
                let rom_off = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
                let bytes = &block[8..];
                chipbox_write(8, OKIM_PSRAM_BASE + rom_off);
                for pair in bytes.chunks(2) {
                    let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
                    chipbox_write(9, w);
                }
            }
            Ok(Event::Write { chip: Chip::K053260, addr, data: d, .. }) => {
                sink.push(OP_EXT | EXT_K060 | (addr as u32) << 8 | d as u32);
            }
            Ok(Event::DataBlock { kind: 0x8E, start, len }) if len >= 8 => {
                // ROM-образ K053260: [u32 полный размер][u32 смещение][данные]
                let block = &data[start..start + len];
                let rom_off = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
                let bytes = &block[8..];
                chipbox_write(8, K060_PSRAM_BASE + rom_off);
                for pair in bytes.chunks(2) {
                    let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
                    chipbox_write(9, w);
                }
            }
            Ok(Event::Write { chip: Chip::NesApu, addr, data: d, .. }) if addr <= 0x1F => {
                sink.push(OP_APU | (addr as u32) << 8 | d as u32);
            }
            Ok(Event::DataBlock { kind: 0xC2, start, len }) if len >= 2 => {
                // DPCM-страница NES: [u16 адрес][данные] — через FIFO,
                // синхронно с потоком (страницы меняются посреди трека)
                let block = &data[start..start + len];
                let a = (block[0] as u32 | (block[1] as u32) << 8) & 0x7FFF;
                sink.push(OP_NESRAM_PTR | a);
                for &b in &block[2..] {
                    sink.push(OP_NESRAM_WR | b as u32);
                }
            }
            Ok(Event::Wait { ticks }) => {
                if ticks > 0 {
                    sink.push(OP_WAIT | ticks as u32);
                }
                match transport(&mut sink.btn, 0, pl) {
                    Some(Ctl::Redraw) => {
                        draw();
                        shown_s = u32::MAX;
                    }
                    Some(ctl) => return ctl,
                    None => {}
                }
                vu_tick(&mut vu_last);
                // время/прогресс раз в секунду + диагностика PSRAM-путей
                let el = elapsed_s();
                if el != shown_s {
                    shown_s = el;
                    ui::progress(el.min(total_s), total_s, "");
                }
            }
            Ok(Event::End) => {
                loops += 1;
                if loops >= 2 || header.loop_offset.is_none() {
                    // дать хвосту FIFO дозвучать
                    while chipbox_status() & 0x1FFF != 0 {
                        match transport(&mut sink.btn, 0, pl) {
                            Some(Ctl::Redraw) => draw(),
                            Some(ctl) => return ctl,
                            None => {}
                        }
                        core::hint::spin_loop();
                    }
                    return Ctl::Next;
                }
                let restart = header.loop_offset.unwrap_or(header.data_offset);
                println!("Луп {loops}");
                reader = Reader::new(data, restart);
            }
            Ok(_) => {} // чужие чипы и блоки данных — пока мимо
            Err(_) => return error_wait("VGM", "stream error"),
        }
    }
}
