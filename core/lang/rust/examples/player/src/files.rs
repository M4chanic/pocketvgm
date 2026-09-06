//! Файловые операции APF поверх слота Music: путь текущего файла
//! (getfile) и открытие произвольного пути (openfile) — база плейлистов.
//! Формат структур сверен с core_bridge_cmd.v и PocketDoom:
//! openfile: { filename[256] нуль-терминированный; u32 flags; u32 size }.

use alloc::string::String;
use alloc::vec::Vec;
use litex_openfpga::litex_pac as pac;

/// Буфер параметр/ответ-структур (SDRAM, между staging и кучей)
const STRUCT_BUF: u32 = 0x4170_0000;

/// Активный data-слот. Слотов ТРИ (Sega / Nintendo / Computer):
/// расширения разнесены между ними из-за лимита APF в 4 расширения на
/// слот. Комментарий про два слота устарел — выбор слота на старте см. в
/// main.rs, там же отпечатки содержимого и заначка в PSRAM.
///
/// Важно для плейлистов: openfile ищет путь В ЭТОМ слоте, поэтому m3u,
/// ссылающийся на файлы другой группы, открыть их не сможет.
static SLOT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
/// Код результата последней файловой операции APF (0 = ok)
static LAST_ERR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// В слоте лежит уже не тот файл, что открывал плеер: базу длительностей
/// SID подгружает тот же слот, и после неё трек надо открывать заново,
/// иначе на повторе в буфер попадёт база вместо музыки.
static SLOT_DIRTY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn mark_dirty() {
    SLOT_DIRTY.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Прочитать и сбросить признак
pub fn take_dirty() -> bool {
    SLOT_DIRTY.swap(false, core::sync::atomic::Ordering::Relaxed)
}

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

/// Структура параметров openfile в блочном ОЗУ ядра (chipbox 0x35/0x36).
///
/// Раскладка APF: имя файла 256 байт с нулём, потом флаги и размер — оба
/// нулевые, мы ничего не создаём и не усекаем.
///
/// Зачем отдельный буфер: APF читает эту структуру ЧЕРЕЗ МОСТ, а чтение
/// SDRAM в базовом ядре идёт через FIFO и Wishbone и отвечает на десятки
/// тактов позже, чем APF успевает забрать данные. Имя файла до APF не
/// доходило — openfile получал пустую строку, отвечал «успех» и не менял
/// содержимое слота. Отсюда все симптомы плейлистов: «трек не найден»,
/// игра прежнего трека вместо выбранного и мусорный список из бинарника.
/// Подробности — в core_top.sv рядом с ofile_buf.
fn push_param_struct(path: &str) {
    let b = path.as_bytes();
    crate::chipbox_write(0x35, 0); // индекс слова, дальше автоинкремент
    for w in 0..66usize {
        let mut v: u32 = 0;
        for k in 0..4usize {
            let i = w * 4 + k;
            if i < b.len() {
                v |= (b[i] as u32) << (8 * k);
            }
        }
        crate::chipbox_write(0x36, v);
    }
}

/// Открыть произвольный путь в слоте. false — файла нет/ошибка.
pub fn open(path: &str) -> bool {
    if path.is_empty() || path.len() > 255 {
        return false;
    }
    push_param_struct(path);
    let p = unsafe { pac::Peripherals::steal() };
    unsafe {
        core::ptr::write_bytes(STRUCT_BUF as *mut u8, 0, 264);
        core::ptr::copy_nonoverlapping(path.as_ptr(), STRUCT_BUF as *mut u8, path.len());
        p.APF_BRIDGE.slot_id.write(|w| w.bits(slot()));
        p.APF_BRIDGE.ram_data_address.write(|w| w.bits(STRUCT_BUF));
        p.APF_BRIDGE.request_openfile.write(|w| w.bits(1));
    }
    // Ответ APF — код результата, и только он. В 0.2.7 здесь стояла
    // дополнительная проверка размера по смещению +260 ответной
    // структуры; она давала ложный отказ НА ЛЮБОМ файле, потому что APF
    // туда ничего не пишет, а буфер мы сами обнуляем перед запросом.
    // Одиночные файлы это не задевало (для них open не вызывается —
    // они уже в слоте), а все треки плейлистов падали с «open err 0»:
    // код APF нулевой, то есть успех, отказ выставляла проверка.
    // Пустой или битый файл ловится дальше по размеру слота и по магии
    // формата, так что защита не потеряна.
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

/// Расширение пути без срезов строки: `path[len-4..]` падает, если путь
/// кончается многобайтовым символом UTF-8 (панику словили на .m3u с
/// кириллицей в имени). Сравниваем байты — регистр ASCII не важен.
pub fn has_ext(path: &str, ext: &str) -> bool {
    let (b, e) = (path.as_bytes(), ext.as_bytes());
    b.len() > e.len() && b[b.len() - e.len()..].eq_ignore_ascii_case(e)
}

/// Хвост строки длиной не больше n байт, обрезанный по границе символа
pub fn tail(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let b = s.as_bytes();
    let mut i = s.len() - n;
    while i < s.len() && (b[i] & 0xC0) == 0x80 {
        i += 1;
    }
    core::str::from_utf8(&b[i..]).unwrap_or("")
}

/// Слоты, в которых APF разрешит открыть файл с таким расширением.
///
/// Списки — из data.json: домашнее ядро [vgm,vgz,gym,m3u] и
/// [vgm,vgz,nsf,gbs], аркадное — один слот [vgm,vgz,m3u]. openfile ищет
/// путь В СЛОТЕ, поэтому плейлист, попавший не в тот слот, не откроет ни
/// одного трека. Текущий слот идёт первым.
///
/// Третьего слота (mid/sid) больше нет: SID и MIDI убраны, см. заметку у
/// M4_HAS_SID в chipbox.sv.
pub fn slots_for(path: &str) -> Vec<u32> {
    const SLOTS: [(u32, &[&str]); 2] = [
        (1, &[".vgm", ".vgz", ".gym", ".m3u"]),
        (2, &[".vgm", ".vgz", ".nsf", ".gbs"]),
    ];
    let mut out: Vec<u32> = Vec::new();
    for (id, exts) in SLOTS.iter() {
        if exts.iter().any(|e| has_ext(path, e)) {
            out.push(*id);
        }
    }
    let cur = slot();
    if let Some(i) = out.iter().position(|&s| s == cur) {
        out.swap(0, i);
    }
    out
}

/// Соседи трека по индексу библиотеки.
///
/// Индекс — `index.txt`, который пишет апдейтер: по пути на строку
/// относительно папки индекса, строки с `#` — комментарии. Ищем строку,
/// которой ОКАНЧИВАЕТСЯ путь трека (с косой чертой перед ней, чтобы
/// «Music/A/01.vgz» не совпал с «…/B/A/01.vgz»); из совпавших берём самую
/// длинную. Всё, что в пути до неё, — префикс: он взят из ответа getfile,
/// и какой бы вид пути ни ждал openfile, соседи получат ровно тот же.
/// Возвращает список путей папки и номер текущего трека в нём.
pub fn siblings_from_index(text: &[u8], own: &str) -> Option<(Vec<String>, usize)> {
    let ob = own.as_bytes();
    let lines = || {
        text.split(|&b| b == b'\n').map(|l| {
            let l = if l.last() == Some(&b'\r') { &l[..l.len() - 1] } else { l };
            l
        })
    };
    // 1. строка трека
    let mut best: &[u8] = &[];
    for l in lines() {
        if l.is_empty() || l[0] == b'#' || l.len() > ob.len() || l.len() <= best.len() {
            continue;
        }
        let at = ob.len() - l.len();
        if &ob[at..] == l && (at == 0 || ob[at - 1] == b'/') {
            best = l;
        }
    }
    if best.is_empty() {
        return None;
    }
    let prefix = &ob[..ob.len() - best.len()];
    let dir_len = best.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    let dir = &best[..dir_len];
    // 2. все строки той же папки
    let mut out: Vec<String> = Vec::new();
    let mut idx = 0usize;
    for l in lines() {
        if l.is_empty() || l[0] == b'#' || l.len() <= dir_len || &l[..dir_len] != dir {
            continue;
        }
        if l[dir_len..].contains(&b'/') {
            continue; // вложенная папка — не сосед
        }
        if l == best {
            idx = out.len();
        }
        let mut full = Vec::with_capacity(prefix.len() + l.len());
        full.extend_from_slice(prefix);
        full.extend_from_slice(l);
        if full.len() <= 255 {
            out.push(String::from_utf8_lossy(&full).into_owned());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some((out, idx))
    }
}

/// Начало строки длиной не больше n байт, обрезанное по границе символа
/// (срез `s[..n]` паникует на многобайтовом символе — см. tail)
pub fn head(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let b = s.as_bytes();
    let mut i = n;
    while i > 0 && (b[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    core::str::from_utf8(&b[..i]).unwrap_or("")
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
