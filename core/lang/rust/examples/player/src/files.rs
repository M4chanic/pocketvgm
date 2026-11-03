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
/// Размер файла из 008A, следующего за последним успешным openfile.
/// Отдельный флажок вместо Option: AtomicU32::new нельзя задать как None.
static LAST_OPEN_SIZE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static LAST_OPEN_VALID: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn set_open_size(size: u32) {
    LAST_OPEN_SIZE.store(size, core::sync::atomic::Ordering::Relaxed);
    LAST_OPEN_VALID.store(true, core::sync::atomic::Ordering::Relaxed);
}

fn clear_open_size() {
    LAST_OPEN_VALID.store(false, core::sync::atomic::Ordering::Relaxed);
}

/// Размер из 008A последнего успешного openfile. После read взять его
/// обязательно: это актуальный размер именно открытого файла, тогда как
/// File::size() может ещё возвращать предыдущий слот/размер из mailbox.
pub fn take_open_size() -> Option<u32> {
    if LAST_OPEN_VALID.swap(false, core::sync::atomic::Ordering::Relaxed) {
        Some(LAST_OPEN_SIZE.load(core::sync::atomic::Ordering::Relaxed))
    } else {
        None
    }
}

/// Размер последнего обновления слота (для файла, выбранного пользователем
/// до старта плеера). Возвращает Some только когда id совпал с запрошенным.
pub fn last_update_size(slot_id: u32) -> Option<u32> {
    if upd_id() == slot_id {
        Some(upd_size())
    } else {
        None
    }
}

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

/// Короткая пауза по 44.1-кГц счётчику chipbox. Нужна между командами APF:
/// сразу после 008A read иногда попадает в переходное состояние слота.
fn wait_ms(ms: u32) {
    let t0 = crate::chipbox_read(0x18);
    while crate::chipbox_read(0x18).wrapping_sub(t0) < ms * 44_100 {
        core::hint::spin_loop();
    }
}

fn wait_op() -> bool {
    let p = unsafe { pac::Peripherals::steal() };
    // Максимум файла в staging — 7 МБ, а APF-перекачка идёт всего несколько
    // МБ/с. Старый предел ~1 с обрывал нормальное чтение, после чего новая
    // команда запускалась поверх ещё не закончившейся перекачки. Десяти
    // секунд хватает с большим запасом; бесконечный цикл не возвращаем,
    // чтобы пустой/зависший запрос всё же освобождал плеер.
    let t0 = crate::chipbox_read(0x18);
    loop {
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
        if crate::chipbox_read(0x18).wrapping_sub(t0) > 10 * 44_100 {
            break;
        }
        core::hint::spin_loop();
    }
    // 99 — наш таймаут, не код APF (у того поле результата трёхбитное).
    LAST_ERR.store(99, core::sync::atomic::Ordering::Relaxed);
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

/// Запасной слот под openfile (id 3 в data.json, параметры 0x8), как у
/// ядер Mega CD, PC Engine CD и CD-i. До 0.2.10 openfile работал и в
/// слоте пользователя; отказ с кодом 4 в 0.2.11–0.2.16 был не из-за
/// слота, а из-за отдачи структуры без конвейерного сдвига (см. ofile_q
/// в core_top.sv). Служебный слот оставлен на случай, если слот
/// пользователя откажет.
const OPEN_SLOT: u32 = 3;
/// Слот, который выбрал пользователь через Load
static USER_SLOT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
/// Код APF от попытки в слоте пользователя — для экрана ошибки
static USER_SLOT_ERR: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub fn set_user_slot(s: u32) {
    USER_SLOT.store(s, core::sync::atomic::Ordering::Relaxed);
    set_slot(s);
}

pub fn user_slot_err() -> u32 {
    USER_SLOT_ERR.load(core::sync::atomic::Ordering::Relaxed)
}

/// Открыть произвольный путь. false — файла нет/ошибка.
///
/// Сначала в слот пользователя, как до 0.2.10, при отказе — в
/// служебный. Удачный слот остаётся активным: из него дальше читает
/// load_slot. Коды обеих попыток видны на экране ошибки.
pub fn open(path: &str) -> bool {
    clear_open_size();
    let c0 = bridge_reads();
    let user_slot = USER_SLOT.load(core::sync::atomic::Ordering::Relaxed);
    set_slot(user_slot);
    let u0 = upd_count();
    let ok = open_as(path);
    let c1 = bridge_reads();
    READS_USER.store(c1.wrapping_sub(c0) & 0xFFFF, core::sync::atomic::Ordering::Relaxed);
    if ok {
        // 008A даёт когерентный размер, но при быстрых переключениях read
        // сразу за ним всё ещё может попасть в переходное состояние слота.
        // Подтверждаем getfile точным путём и даём слоту короткую паузу.
        if wait_slot_update(u0, user_slot) {
            wait_ms(20);
            for _ in 0..20 {
                if slot_path() == path && upd_id() == user_slot {
                    set_open_size(upd_size());
                    wait_ms(10);
                    break;
                }
                wait_ms(10);
            }
        }
        return true;
    }
    USER_SLOT_ERR.store(last_err(), core::sync::atomic::Ordering::Relaxed);
    set_slot(OPEN_SLOT);
    let ok = open_as(path);
    READS_OPEN.store(bridge_reads().wrapping_sub(c1) & 0xFFFF, core::sync::atomic::Ordering::Relaxed);
    if !ok {
        set_slot(USER_SLOT.load(core::sync::atomic::Ordering::Relaxed));
    }
    ok
}

/// Счётчик уведомлений 008A «слот обновлён» (chipbox 0x20, биты 23:16)
fn upd_count() -> u32 {
    (crate::chipbox_read(0x20) >> 16) & 0xFF
}

/// Id слота последнего 008A (chipbox 0x20, младшие 16 бит)
fn upd_id() -> u32 {
    crate::chipbox_read(0x20) & 0xFFFF
}

/// Размер последнего 008A (chipbox 0x21), когерентен полю из 0x20
fn upd_size() -> u32 {
    crate::chipbox_read(0x21)
}

/// Дождаться 008A с новым размером файла после openfile. true — пришло
/// именно ожидаемое обновление нужного слота; false — только таймаут.
///
/// Завершение openfile не означает, что Pocket уже обновил путь, размер и
/// содержимое слота. getfile сразу за ним тоже может вернуть прежний путь,
/// поэтому подтверждать открытие так нельзя. Для user-reloadable слота
/// надёжный маркер — отдельное уведомление 008A. Обновление ДРУГОГО слота
/// не считаем: иначе можно начать чтение до смены файла.
fn wait_slot_update(u0: u32, slot_id: u32) -> bool {
    let t0 = crate::chipbox_read(0x18); // счётчик 44100 Гц
    for _ in 0..10_000_000u32 {
        let info = crate::chipbox_read(0x20);
        let count = (info >> 16) & 0xFF;
        let id = info & 0xFFFF;
        if count != u0 && id == slot_id {
            return true;
        }
        if crate::chipbox_read(0x18).wrapping_sub(t0) > 44_100 {
            return false;
        }
    }
    false
}

/// Сколько слов APF прочитал через мост за попытку в слоте пользователя
/// и в служебном. Структура — 66 слов, APF читает её 67 чтениями плюс
/// повторы флагов; 0 значит, что APF её не читал вовсе.
static READS_USER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static READS_OPEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Счётчик чтений моста вне региона команд (chipbox 0x38, см. obr_cnt в
/// core_top.sv)
fn bridge_reads() -> u32 {
    crate::chipbox_read(0x38)
}

/// Слово структуры так, как его должен получить APF: первый символ в
/// старшем байте (мост big-endian, см. ofile_rd_data в core_top)
fn expected_word(path: &[u8], w: usize) -> u32 {
    let mut v: u32 = 0;
    for k in 0..4usize {
        let i = w * 4 + k;
        if i < path.len() {
            v |= (path[i] as u32) << (24 - 8 * k);
        }
    }
    v
}

/// Сверка снимка (chipbox 0x3C) с тем, что фирмварь записала для пути:
/// «rb:ok» или «rb:@<слово> <получено>» на первом расхождении.
pub fn struct_check(path: &str) -> String {
    let b = path.as_bytes();
    for w in 0..66usize {
        crate::chipbox_write(0x3C, w as u32);
        let _ = crate::chipbox_read(0x3C); // ОЗУ снимка отвечает тактом позже
        let got = crate::chipbox_read(0x3C);
        if got != expected_word(b, w) {
            return alloc::format!("rb:@{} {:08x}", w, got);
        }
    }
    String::from("rb:ok")
}

/// Порядок последних n чтений моста словами структуры, слитый в
/// диапазоны: «0-65 0-4» значит, что APF прочитал её целиком, а потом
/// ещё раз первые пять слов.
pub fn read_order(n: u32) -> String {
    let end = bridge_reads();
    let n = n.min(128);
    let mut out = String::from("o:");
    let mut run: Option<(u32, u32)> = None;
    let flush = |out: &mut String, r: (u32, u32)| {
        if out.len() > 2 {
            out.push(' ');
        }
        if r.0 == r.1 {
            out.push_str(&alloc::format!("{}", r.0));
        } else {
            out.push_str(&alloc::format!("{}-{}", r.0, r.1));
        }
    };
    for k in 0..n {
        let i = end.wrapping_sub(n).wrapping_add(k) & 127;
        crate::chipbox_write(0x3C, i);
        let _ = crate::chipbox_read(0x3D);
        let w = (crate::chipbox_read(0x3D) & 0x1FF) >> 2;
        run = match run {
            Some((a, z)) if w == z + 1 => Some((a, w)),
            Some(r) => {
                flush(&mut out, r);
                Some((w, w))
            }
            None => Some((w, w)),
        };
    }
    if let Some(r) = run {
        flush(&mut out, r);
    }
    out
}

/// Проба: код APF на openfile этого пути в служебном слоте
pub fn probe(path: &str) -> u32 {
    set_slot(USER_SLOT.load(core::sync::atomic::Ordering::Relaxed));
    if open_as(path) {
        mark_dirty(); // в слоте теперь проба, а не трек
    }
    last_err()
}

/// Сколько чтений моста ушло на последнюю попытку (служебный слот)
pub fn reads_last() -> u32 {
    READS_OPEN.load(core::sync::atomic::Ordering::Relaxed)
}

/// Журнал чтений моста для экрана ошибки openfile, две строки:
/// «r:<слот пользователя>/<служебный> a:<последний адрес>» и
/// «w:<слово 0> <слово 1>» — что APF получил в начале структуры. Путь
/// «/Assets/…» должен прийти как 2f417373 6574732f.
pub fn bridge_log() -> (String, String) {
    let ro = READS_OPEN.load(core::sync::atomic::Ordering::Relaxed);
    let ru = READS_USER.load(core::sync::atomic::Ordering::Relaxed);
    (
        alloc::format!("r:{}/{} a:{:08x}", ru, ro, crate::chipbox_read(0x39)),
        alloc::format!("w:{:08x} {:08x}", crate::chipbox_read(0x3A), crate::chipbox_read(0x3B)),
    )
}

fn open_as(path: &str) -> bool {
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
    // Здесь завершается только сама команда openfile. Актуализации слота
    // ждёт вызывающая сторона через 008A; немедленный getfile возвращает
    // прежний путь и ошибочно превращает успешное открытие в open err 0/0.
    wait_op()
}

/// Чтение из произвольного слота с таймаутом (block_op_complete litex
/// может зависнуть навечно на пустом слоте). Код APF 2 («error/out of range»)
/// сразу после openfile бывает переходным: один повтор через короткую паузу.
pub fn read_slot_to(slot_id: u32, len: u32, addr: u32) -> bool {
    let p = unsafe { pac::Peripherals::steal() };
    for attempt in 0..2 {
        unsafe {
            p.APF_BRIDGE.slot_id.write(|w| w.bits(slot_id));
            p.APF_BRIDGE.data_offset.write(|w| w.bits(0));
            p.APF_BRIDGE.transfer_length.write(|w| w.bits(len));
            p.APF_BRIDGE.ram_data_address.write(|w| w.bits(addr));
            p.APF_BRIDGE.request_read.write(|w| w.bits(1));
        }
        if wait_op() {
            return true;
        }
        if last_err() != 2 || attempt == 1 {
            return false;
        }
        wait_ms(20);
    }
    false
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
