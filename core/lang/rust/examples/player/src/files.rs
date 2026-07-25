//! Файловые операции APF поверх слота Music: путь текущего файла
//! (getfile) и открытие произвольного пути (openfile) — база плейлистов.
//! Формат структур сверен с core_bridge_cmd.v и PocketDoom:
//! openfile: { filename[256] нуль-терминированный; u32 flags; u32 size }.

use alloc::string::String;
use alloc::vec::Vec;
use litex_openfpga::litex_pac as pac;

/// Буфер параметр/ответ-структур (SDRAM, между staging и кучей)
const STRUCT_BUF: u32 = 0x4170_0000;

/// Активный data-слот (1 — vgm/vgz/gbs/m3u, 2 — nsf/sid/mid): расширения
/// разнесены по двум слотам из-за лимита APF в 4 расширения на слот
static SLOT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
/// Код результата последней файловой операции APF (0 = ok)
static LAST_ERR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub fn last_err() -> u32 {
    LAST_ERR.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn set_slot(s: u32) {
    SLOT.store(s, core::sync::atomic::Ordering::Relaxed);
}

pub fn slot() -> u32 {
    SLOT.load(core::sync::atomic::Ordering::Relaxed)
}

fn wait_op() -> bool {
    let p = unsafe { pac::Peripherals::steal() };
    for _ in 0..60_000_000u32 {
        let st = unsafe { p.APF_BRIDGE.status.read().bits() };
        if st != 0 {
            if st & 1 == 0 {
                return false;
            }
            // сам запрос завершён — успех определяет код результата APF
            // (0 = ok; для несуществующего файла запрос тоже «завершается»)
            let code = unsafe { p.APF_BRIDGE.command_result_code.read().bits() };
            LAST_ERR.store(code, core::sync::atomic::Ordering::Relaxed);
            return code == 0;
        }
    }
    false
}

/// Путь файла, выбранного в слоте (пустая строка при ошибке)
pub fn slot_path() -> String {
    let p = unsafe { pac::Peripherals::steal() };
    unsafe {
        core::ptr::write_bytes(STRUCT_BUF as *mut u8, 0, 264);
        p.APF_BRIDGE.slot_id.write(|w| w.bits(slot()));
        p.APF_BRIDGE.ram_data_address.write(|w| w.bits(STRUCT_BUF));
        p.APF_BRIDGE.request_getfile.write(|w| w.bits(1));
    }
    if !wait_op() {
        return String::new();
    }
    let raw = unsafe { core::slice::from_raw_parts(STRUCT_BUF as *const u8, 256) };
    let len = raw.iter().position(|&b| b == 0).unwrap_or(255);
    String::from_utf8_lossy(&raw[..len]).into_owned()
}

/// Открыть произвольный путь в слоте. false — файла нет/ошибка.
/// ВАЖНО: сам запрос «успешен» и для отсутствующего файла — признак
/// реального открытия только валидный размер в ответ-структуре (+260).
pub fn open(path: &str) -> bool {
    if path.is_empty() || path.len() > 255 {
        return false;
    }
    let p = unsafe { pac::Peripherals::steal() };
    unsafe {
        core::ptr::write_bytes(STRUCT_BUF as *mut u8, 0, 264);
        core::ptr::copy_nonoverlapping(path.as_ptr(), STRUCT_BUF as *mut u8, path.len());
        p.APF_BRIDGE.slot_id.write(|w| w.bits(slot()));
        p.APF_BRIDGE.ram_data_address.write(|w| w.bits(STRUCT_BUF));
        p.APF_BRIDGE.request_openfile.write(|w| w.bits(1));
    }
    wait_op()
}

/// Чтение из произвольного слота с таймаутом (block_op_complete litex
/// может зависнуть навечно на пустом слоте)
pub fn read_slot_to(slot_id: u32, len: u32, addr: u32) -> bool {
    let p = unsafe { pac::Peripherals::steal() };
    unsafe {
        p.APF_BRIDGE.slot_id.write(|w| w.bits(slot_id));
        p.APF_BRIDGE.data_offset.write(|w| w.bits(0));
        p.APF_BRIDGE.transfer_length.write(|w| w.bits(len));
        p.APF_BRIDGE.ram_data_address.write(|w| w.bits(addr));
        p.APF_BRIDGE.request_read.write(|w| w.bits(1));
    }
    wait_op()
}

/// Каталог из пути ("a/b/c.vgm" -> "a/b/")
pub fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i + 1],
        None => "",
    }
}

/// Разбор m3u: непустые строки без ведущего '#', пути относительно base
pub fn parse_m3u(text: &[u8], base: &str) -> Vec<String> {
    // UTF-8 BOM в начале файла — не часть первого пути
    let text = if text.starts_with(&[0xEF, 0xBB, 0xBF]) { &text[3..] } else { text };
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let line: &[u8] = if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { line };
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        // трим пробелов/табов по краям (плейлисты бывают с хвостовыми)
        let mut a = 0;
        let mut z = line.len();
        while a < z && (line[a] == b' ' || line[a] == b'\t') {
            a += 1;
        }
        while z > a && (line[z - 1] == b' ' || line[z - 1] == b'\t') {
            z -= 1;
        }
        let line = &line[a..z];
        if line.is_empty() {
            continue;
        }
        // защита от бинарного мусора: управляющие байты — не путь
        if line.iter().any(|&b| b < 0x20) {
            continue;
        }
        let s = String::from_utf8_lossy(line);
        // только известные расширения — плейлист не может ссылаться на прочее
        let low: String = s.chars().map(|c| c.to_ascii_lowercase()).collect();
        let known = [".vgm", ".vgz", ".gym", ".nsf", ".gbs", ".sid", ".mid"];
        if !known.iter().any(|e| low.ends_with(e)) {
            continue;
        }
        let mut full = String::from(base);
        full.push_str(&s);
        if full.len() <= 255 {
            out.push(full);
        }
    }
    out
}
