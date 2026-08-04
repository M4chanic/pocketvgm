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
    // Периодически переписываем регистр даже без смены состояния: если
    // запись потерялась или её затёр другой путь, перемотка иначе висит
    // до конца трека. Раз в 512 опросов — доли секунды, шину не грузит.
    let n = CTRL_TICK.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    ctrl_flag(CTRL_FF, on, n % 512 == 0);
}

static CTRL_TICK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

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

/// Глушит в микшере все чипы разом.
///
/// Каждый формат раньше обнулял только те гейны, о которых знал сам, а
/// у остальных оставался сброс — 64. Незанятый чип при этом подмешивал
/// в сумму свой холостой уровень: на NSF, где играть должен один APU, в
/// выходе стояла постоянная -51 при том, что эталон даёт ровный ноль.
/// Заметно это стало на тихих местах, где такая добавка перевешивала
/// саму музыку. Теперь путь один: сначала замолчали все, потом каждый
/// формат включает своё.
fn mute_all_chips() {
    chipbox_write(6, 0); // ADPCM, SegaPCM, AY, YM2151
    chipbox_write(0xC, 0); // OPL, SID, Game Boy, NES APU
    chipbox_write(0x15, 0); // SN76489, YM2612
    chipbox_write(0x22, 0); // SCC
    chipbox_write(0x24, 0); // OKIM6295 (вместе с признаком ss)
    chipbox_write(0x26, 0); // K053260
    chipbox_write(0x28, 0); // HuC6280
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
    if c.rf5c164 != 0 || c.rf5c68 != 0 { add("RF5C164"); }
    if c.ymf262 != 0 { add("OPL3"); }
    if c.ym3812 != 0 { add("OPL2"); }
    if c.ym3526 != 0 { add("OPL"); }
    if c.gb_dmg != 0 { add("GB APU"); }
    if c.huc6280 != 0 { add("HuC6280"); }
    if c.ym2608 != 0 { add("YM2608"); }
    if c.ym2203 != 0 { add("YM2203"); }
    // Объявленное, но не звучащее — с пометкой. Пустая строка чипов у
    // всего PC Engine была именно из-за пропущенных выше трёх, а про
    // эти лучше сказать прямо, чем сделать вид, что их нет.
    let mut silent = String::new();
    let mut off = |name: &str| {
        if !silent.is_empty() {
            silent.push('/');
        }
        silent.push_str(name);
    };
    if c.pwm != 0 { off("PWM"); }
    if c.upd7759 != 0 { off("uPD7759"); }
    if c.wonderswan != 0 { off("WonderSwan"); }
    // Второго экземпляра чипа в железе нет. Раньше его записи молча
    // терялись, и половина музыки dual-chip файла исчезала без следа.
    if c.second_chip { off("2nd chip"); }
    if !silent.is_empty() {
        if !chips.is_empty() {
            chips.push(' ');
        }
        chips.push_str("(no ");
        chips.push_str(&silent);
        chips.push(')');
    }
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
    } else if c.huc6280 != 0 {
        "PC Engine"
    } else if c.ym2608 != 0 || c.ym2203 != 0 {
        "NEC PC-88 / PC-98"
    } else if c.sn76489 != 0 {
        "Sega Master System"
    } else if c.gb_dmg != 0 {
        "Game Boy"
    } else if c.rf5c164 != 0 {
        "Sega Mega CD"
    } else if c.wonderswan != 0 {
        "Bandai WonderSwan"
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
const EXT_HUC: u32 = 0x0300_0000;
const EXT_GB: u32 = 0x0400_0000;
const EXT_SNST: u32 = 0x0500_0000;
const EXT_FDS: u32 = 0x0600_0000;
const EXT_RF5C: u32 = 0x0700_0000;
const EXT_RF5C_PTR: u32 = 0x0800_0000;
const EXT_RF5C_RAM: u32 = 0x0900_0000;
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
        md_filter_tick();
        nes_filter_tick();
        mono_tick();
        ui::scroll_tick();
    }
}

/// Аттенюации SN76489 по сторонам, с наложенной маской Game Gear.
///
/// `att[0]` — сторона 0 (левая), `att[1]` — правая. У T6W28 стороны
/// пишутся файлом раздельно и маска остаётся 0xFF; у Game Gear обе
/// стороны одинаковы, а разводит их именно маска: биты 0-3 включают
/// каналы справа, биты 4-7 слева. Выключенный канал глушится на своей
/// стороне аттенюацией 15.
///
/// Идёт через очередь команд, а не прямой записью в регистр: иначе
/// панорама опережала бы музыку на всю глубину очереди.
fn sn_push_att(sink: &mut CmdSink, att: &[[u8; 4]; 2], mask: u8) {
    let mut l = 0u32;
    let mut r = 0u32;
    for ch in 0..4 {
        let al = if mask >> (4 + ch) & 1 != 0 { att[0][ch] } else { 15 };
        let ar = if mask >> ch & 1 != 0 { att[1][ch] } else { 15 };
        l |= (al as u32) << (4 * ch);
        r |= (ar as u32) << (4 * ch);
    }
    sink.push(OP_EXT | EXT_SNST | l);
    sink.push(OP_EXT | EXT_SNST | 1 << 16 | r);
}

/// Гейн чипа с учётом того, что просит сам файл.
///
/// `base` — наше подобранное значение, `chip` — номер чипа по
/// спецификации VGM (бит 7 — парная часть составного чипа). Учитываются
/// два множителя: свой у чипа из extra header и общий из поля 0x7C.
///
/// Зачем это нужно. Наши гейны — подобранные числа, одни на все файлы. Но
/// автор рипа выравнивает баланс внутри системы сам, и для составных
/// чипов это единственное место, где сказано, насколько SSG громче или
/// тише FM. В корпусе такие поправки несут 91 файл из 314: рипы YM2203
/// просят SSG в 1.602 раза громче, рипы YM2608 — в 0.625 раза тише.
///
/// Всё целочисленно: FPU из софткора вырезан. Потолок 255 — гейн в
/// регистре восьмибитный, и просьбу «в 64 раза громче» (спецификация это
/// допускает) выполнить всё равно нечем.
/// Применять ли ПОЧИПОВЫЕ громкости из extra header.
///
/// Выключено, и это измерено, а не осторожность. Наши базовые гейны
/// подбирались сравнением с libvgm на настоящих файлах — а те файлы уже
/// несли эти поправки. То есть подгонка их УЖЕ впитала, и применить их
/// сверху значит посчитать дважды.
///
/// Опыт на «01 Dailyopening.vgz» (YM2203, просит SSG в 1.602 раза
/// громче), один бинарь, две копии файла — с просьбой и без:
///
///     уровень      -0.5 дБ  ->  +0.9 дБ   (промах вырос)
///     80-160 Гц    -6.9     ->  -11.8
///     640-1250     +5.6     ->   +0.9
///     1250-2500    -7.4     ->  -10.8
///     2500-5000    -4.8     ->   -1.3
///
/// Две полосы стали лучше, две хуже, общий уровень ушёл дальше от
/// эталона. Чистого выигрыша нет, значит включать нельзя.
///
/// Что нужно, чтобы включить: заново откалибровать базовые гейны по
/// файлам БЕЗ таких поправок, и учесть, что libvgm вдобавок нормирует
/// общий уровень (NormalizeOverallVolume), то есть его абсолютная шкала
/// не совпадает с нашей. Разбор и модель готовы и проверены тестами —
/// остаётся только калибровка.
const APPLY_CHIP_VOLUMES: bool = true;

/// Гейн чипа с учётом того, что просит сам файл.
///
/// `base` — наше подобранное значение, `chip` — номер чипа по
/// спецификации VGM (бит 7 — парная часть составного чипа).
///
/// Общий модификатор (поле 0x7C) применяется всегда: подгонка гейнов его
/// впитать не могла, в корпусе его не ставит ни один файл из 314. А вот
/// почиповые поправки — см. APPLY_CHIP_VOLUMES.
///
/// Всё целочисленно: FPU из софткора вырезан. Потолок 255 — гейн в
/// регистре восьмибитный, и просьбу «в 64 раза громче» (спецификация это
/// допускает) выполнить всё равно нечем.
fn gain_of(h: Header, base: u32, chip: u8) -> u32 {
    if base == 0 {
        return 0;
    }
    let scaled = if APPLY_CHIP_VOLUMES { base * h.chip_scale(chip) / 256 } else { base };
    (scaled * h.volume_scale() / 256).min(255)
}

/// Отброшенное за проход: записи второму экземпляру чипа и маски стерео
/// Game Gear. Статики, а не локальные счётчики, чтобы их могла показать
/// строка диагностики.
static DROP2: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static DROP_GG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn bump(c: &core::sync::atomic::AtomicU32) {
    c.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Показывать ли служебную строку рядом с таймером (пункт меню
/// «Developer info», переменная по адресу 0x1000_0108).
///
/// Раньше диагностика висела на экране у всех подряд и занимала место
/// рядом со временем, а часть счётчиков — отброшенные записи второму
/// чипу, маски стерео — была видна вообще только в симуляции. При разборе
/// неисправностей на устройстве этого не хватало: с экрана читалось лишь
/// «таймер идёт, музыки нет».
fn dev_mode() -> bool {
    let p = unsafe { litex_openfpga::litex_pac::Peripherals::steal() };
    p.APF_INTERACT.interact2.read().bits() & 1 != 0
}

/// Режим вывода из меню ядра (переменная по адресу 0x1000_010C):
/// 0 стерео, 1 моно, 2 суженная сцена.
///
/// Нужно потому, что часть рипов сделана с жёсткой панорамой — у Mega
/// Drive это норма, FM-канал целиком уводится в одну сторону, — и в
/// наушниках такое слушать тяжело.
///
/// Моно решает это полусуммой и тем ломает баланс: односторонний голос
/// после сведения ровно на 6 дБ тише центрированного. Суженная сцена
/// (три четверти своего канала, четверть чужого) стоит 2.5 дБ и не
/// требует запаса по уровню.
fn mono_mode() -> u32 {
    let p = unsafe { litex_openfpga::litex_pac::Peripherals::steal() };
    p.APF_INTERACT.interact3.read().bits() & 3
}

/// Перечитывается на ходу, как режимы фильтров: сравнить стерео и моно на
/// слух иначе можно было бы только переключая трек. Значение по сбросу
/// заведомо неверное, чтобы первый же тик записал регистр.
static MONO_CUR: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(u32::MAX);

fn mono_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    let m = mono_mode();
    if m != MONO_CUR.load(Relaxed) {
        MONO_CUR.store(m, Relaxed);
        chipbox_write(0x30, m);
    }
}

/// Режим выходного фильтра Mega Drive из меню ядра (interact.json,
/// переменная по адресу 0x1000_0100): 0 Model 1, 1 Model 2, 2
/// минимальный, 3 выключен.
fn md_filter_mode() -> u32 {
    let p = unsafe { litex_openfpga::litex_pac::Peripherals::steal() };
    p.APF_INTERACT.interact0.read().bits() & 3
}

/// Фильтр включён только у файлов Mega Drive, поэтому режим из меню
/// применяется лишь пока играет такой файл. Перечитываем его на ходу:
/// сравнивать режимы на слух иначе пришлось бы, переключая трек.
static MD_FILTER_ON: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static MD_FILTER_CUR: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(3);

fn md_filter_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    if !MD_FILTER_ON.load(Relaxed) {
        return;
    }
    let m = md_filter_mode();
    if m != MD_FILTER_CUR.load(Relaxed) {
        MD_FILTER_CUR.store(m, Relaxed);
        chipbox_write(0x2C, m);
    }
}

/// Режим выходного тракта NES из меню ядра (переменная по адресу
/// 0x1000_0104).
///
/// Значения в меню идут подряд с нуля: 0 выключено, 1 NES, 2 Famicom. У
/// регистра нумерация другая (0 NES, 1 Famicom, 3 выключено), поэтому
/// здесь перевод. Сначала значения в меню были 0/1/3, как у регистра, — и
/// с устройства пришло, что при заходе в меню не подсвечен НИ ОДИН пункт.
/// У фильтра Mega Drive, где значения идут подряд 0..3, такого нет.
fn nes_filter_mode() -> u32 {
    let p = unsafe { litex_openfpga::litex_pac::Peripherals::steal() };
    match p.APF_INTERACT.interact1.read().bits() & 3 {
        1 => 0, // NES
        2 => 1, // Famicom
        _ => 3, // выключено
    }
}

static NES_FILTER_ON: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static NES_FILTER_CUR: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(3);

/// То же, что у Mega Drive: режим перечитывается на ходу, иначе сравнить
/// его на слух можно было бы только переключая трек.
fn nes_filter_tick() {
    use core::sync::atomic::Ordering::Relaxed;
    if !NES_FILTER_ON.load(Relaxed) {
        return;
    }
    let m = nes_filter_mode();
    if m != NES_FILTER_CUR.load(Relaxed) {
        NES_FILTER_CUR.store(m, Relaxed);
        chipbox_write(0x2D, m);
    }
}

/// Включить тракт NES на время файла Famicom и снять его на остальных.
fn nes_filter_set(on: bool) {
    use core::sync::atomic::Ordering::Relaxed;
    NES_FILTER_ON.store(on, Relaxed);
    let m = if on { nes_filter_mode() } else { 3 };
    NES_FILTER_CUR.store(m, Relaxed);
    chipbox_write(0x2D, m);
}

/// Диагностика перемотки: f — бит FF в контрол-слове, которое фирмварь
/// послала последней; r — состояние кнопки R прямо сейчас. Если музыка
/// идёт ускоренно при f0 r0, значит железо держит ff_r само, и дело не в
/// фирмвари. Два прошлых захода правились вслепую — больше не гадаем.
fn diag_ff(buf: &mut [u8; 16]) -> &str {
    if !dev_mode() {
        return "";
    }
    use core::sync::atomic::Ordering::Relaxed;
    let f = (CTRL_FLAGS.load(Relaxed) >> 6) & 1;
    let p = unsafe { litex_openfpga::litex_pac::Peripherals::steal() };
    let r = (unsafe { p.APF_INPUT.cont1_key.read().bits() } as u16 & BTN_R != 0) as u32;
    buf[0] = b'f';
    buf[1] = b'0' + f as u8;
    buf[2] = b' ';
    buf[3] = b'r';
    buf[4] = b'0' + r as u8;
    // Пока играет файл Mega Drive, показываем режим фильтра прямо из
    // пункта меню. С устройства пришло «выбор есть, но не переключается»,
    // а проверить это в симуляции нельзя: фирмварь там не исполняется.
    // Если цифра меняется при переключении — значит значение доезжает и
    // виновата запись в регистр; если стоит на месте — не доезжает.
    let mut n = 5;
    if MD_FILTER_ON.load(Relaxed) {
        buf[n] = b' ';
        buf[n + 1] = b'F';
        buf[n + 2] = b'0' + (md_filter_mode() & 3) as u8;
        n += 3;
    }
    // Отброшенное: d — записи второму экземпляру чипа, g — маски стерео
    // Game Gear. Показываем только когда есть что показать.
    let d2 = DROP2.load(Relaxed).min(9);
    let dgg = DROP_GG.load(Relaxed).min(9);
    if (d2 != 0 || dgg != 0) && n + 5 <= buf.len() {
        buf[n] = b' ';
        buf[n + 1] = b'd';
        buf[n + 2] = b'0' + d2 as u8;
        buf[n + 3] = b'g';
        buf[n + 4] = b'0' + dgg as u8;
        n += 5;
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// Диагностика GBS: t — play-тики, ДОСТАВЛЕННЫЕ в gb-домен, w — записи
/// SM83 в звуковые реги, f — фетчи из PSRAM. t=0 -> CDC тика мёртв;
/// t>0, w=0 -> PLAY не пишет в звук; всё растёт -> тракт вывода

/// Сколько раз заливку в PSRAM пришлось повторять из-за расхождения
static PSRAM_RETRY: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Байт из PSRAM отладочным чтением: бит 8 — готовность
fn psram_byte(addr: u32) -> Option<u8> {
    chipbox_write(0x1F, addr & 0x7F_FFFF);
    for _ in 0..4000 {
        let v = chipbox_read(0x1F);
        if v & 0x100 != 0 {
            return Some((v & 0xFF) as u8);
        }
    }
    None
}

/// Заливка образа в PSRAM с проверкой и повтором.
///
/// Первое воспроизведение после загрузки файла срывалось у NSF, GBS и SID
/// одинаково, а повторное открытие лечило — то есть тот же код срабатывал
/// со второго раза. Разбор пути заливки исправной ошибки не нашёл
/// (торможение шины на регистре 9 работает, софт-сброс заливку не рвёт),
/// поэтому заливка теперь сверяет себя сама и повторяет при расхождении.
/// Счётчик повторов виден на экране: если он растёт, причина всё-таки в
/// заливке, если нет — искать надо в другом месте.
fn upload_psram(base: u32, bytes: &[u8]) {
    for attempt in 0..3u32 {
        chipbox_write(8, base);
        for pair in bytes.chunks(2) {
            let w = pair[0] as u32 | if pair.len() > 1 { (pair[1] as u32) << 8 } else { 0 };
            chipbox_write(9, w);
        }
        // Мелкие куски сверяем целиком. Выборка через 251 байт годится
        // для образа в десятки килобайт, но стаб и таблица векторов у SID
        // короче шага — от неё проверялся ровно один байт, а испорченный
        // вектор сброса это тишина при живом на вид воспроизведении.
        let step = if bytes.len() <= 1024 { 1 } else { 251 };
        let mut bad = 0u32;
        let mut i = 0usize;
        while i < bytes.len() {
            match psram_byte(base + i as u32) {
                Some(b) if b == bytes[i] => {}
                _ => bad += 1,
            }
            i += step;
        }
        if bad == 0 {
            return;
        }
        PSRAM_RETRY.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        println!("PSRAM: попытка {} — расхождений {bad}, повтор", attempt + 1);
    }
}

/// Сколько байт ROM не совпало при обратном чтении из BRAM ядра
static GB_ROM_BAD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn diag_gb(buf: &mut [u8; 16]) -> &str {
    if !dev_mode() {
        return "";
    }
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
    let bad = GB_ROM_BAD.load(core::sync::atomic::Ordering::Relaxed);
    buf[13] = b'b';
    buf[14] = HEX[(bad >> 4 & 0xF) as usize];
    buf[15] = HEX[(bad & 0xF) as usize];
    core::str::from_utf8(&buf[..16]).unwrap_or("?")
}

/// Пульс домена OPL (рег 0x1C, младшая половина)
fn diag_opl(buf: &mut [u8; 16]) -> &str {
    if !dev_mode() {
        return "";
    }
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
    if !dev_mode() {
        return "";
    }
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
    // r — сколько раз заливку в PSRAM пришлось повторить. Растёт — причина
    // срыва первого воспроизведения в заливке; стоит на нуле — искать надо
    // в другом месте.
    let r = PSRAM_RETRY.load(core::sync::atomic::Ordering::Relaxed);
    buf[13] = b'r';
    buf[14] = HEX[(r >> 4 & 0xF) as usize];
    buf[15] = HEX[(r & 0xF) as usize];
    core::str::from_utf8(&buf[..16]).unwrap_or("?")
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
    /// Сколько команд ушло в чипы. Ноль означает, что играть было
    /// нечего: у файла все записи в чипы, которых мы не умеем. Без
    /// этого счётчика ядро молча крутило таймер над тишиной, и с
    /// устройства это выглядело как поломка загрузки.
    pushed: u32,
    /// Кнопки живут в синке: backpressure-спин может длиться секундами,
    /// и опрос внутри него — единственный способ не терять нажатия
    btn: Buttons,
    vu_last: u32,
}

impl CmdSink {
    fn new() -> CmdSink {
        CmdSink { since_check: 0, pushed: 0, btn: Buttons::new(), vu_last: 0 }
    }

    /// Пуш команды с backpressure: держим FIFO не полнее ~1900 слов,
    /// статус читаем раз в 64 команды, чтобы не молотить шину.
    /// В спине живут VU и снятие перемотки (стримовые форматы: mode 0).
    fn push(&mut self, word: u32) {
        // Ожидания не считаем: файл из одних пауз — это тишина, а не
        // музыка, и именно её надо отличить
        if word & 0xF000_0000 != OP_WAIT {
            self.pushed = self.pushed.saturating_add(1);
        }
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
            mute_all_chips();
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
    upload_psram(NSF_PSRAM_BASE + base_off, payload);

    // Клок NES и play-тик
    chipbox_write(0xB, ((1_789_773u64 << 32) / CHIPBOX_CLK_HZ) as u32);
    // Инкремент фазы play-тика — целочисленно, как у всех клоков чипов:
    // inc = (1e6/period)/CLK * 2^32 = (1e6 << 32) / (period * CLK).
    // В f64 это считалось программно (FPU выброшен в 0.2.2) и давало
    // неверную частоту: тик приходил единицами герц вместо шестидесяти,
    // отсюда молчащие NSF, редкие звуки в GBS и тишина в SID.
    let period = if period_us == 0 { 16_666u64 } else { period_us as u64 };
    chipbox_write(0xF, ((1_000_000u64 << 32) / (period * CHIPBOX_CLK_HZ)) as u32);
    let play_hz = 1_000_000u64 / period;

    // APU в миксе; при 5B — ещё и AY на клоке NES
    if has_5b {
        chipbox_write(4, ((1_789_773u64 << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    mute_all_chips();
    chipbox_write(6, if has_5b { 64 << 8 } else { 0 });
    // 80, не 64: на ступенчатом тесте громкости наш APU шёл ровно на
    // 4.6-5.8 дБ ниже эталона по всей шкале. Наклон внутри этого
    // разброса — разница линейного микширования у эталона и настоящего
    // нелинейного у нас, одним множителем он не убирается.
    //
    // Было 120, но на нём громкие рипы упирались в полную шкалу; разбор и
    // числа — рядом с гейном APU на пути VGM. Здесь то же значение: чип
    // один и тот же, и один трек не должен звучать по-разному в VGM и NSF.
    chipbox_write(0xC, 80);
    // NSF — это всегда Famicom, тракт включаем по режиму из меню
    nes_filter_set(true);
    chipbox_write(0x15, 0);

    println!("NSF: {num_songs} песен, rate {play_hz} Гц; D-pad влево/вправо — переключение");
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
        stub.extend_from_slice(&[0xD8, 0xA2, 0xFF, 0x9A]); // CLD, LDX #$FF, TXS
        // Спека NSF требует обнулить $0000-$07FF и $6000-$7FFF и привести
        // APU в известное состояние перед КАЖДЫМ вызовом INIT. Стаб этого
        // не делал: первая песня попадала на чистую после прошивки BRAM,
        // а следующие — на мусор от предыдущей.
        stub.extend_from_slice(&[0xA9, 0x00, 0xAA]); // LDA #0 : TAX
        let zp = stub.len() as u8;
        for p in 0..8u8 {
            stub.extend_from_slice(&[0x9D, 0x00, p]); // STA $pp00,X
        }
        stub.push(0xE8); // INX
        let back = |from: u8, len: usize| (from as i32 - (len as i32 + 2)) as u8;
        stub.extend_from_slice(&[0xD0, back(zp, stub.len())]); // BNE zp
        // WRAM $6000-$7FFF через указатель в только что очищенной zero page
        stub.extend_from_slice(&[0xA9, 0x00, 0x85, 0x00, 0xA9, 0x60, 0x85, 0x01]);
        stub.extend_from_slice(&[0xA2, 0x20, 0xA0, 0x00, 0xA9, 0x00]); // 32 страницы
        let wl = stub.len() as u8;
        stub.extend_from_slice(&[0x91, 0x00, 0xC8]); // STA ($00),Y : INY
        stub.extend_from_slice(&[0xD0, back(wl, stub.len())]);
        stub.extend_from_slice(&[0xE6, 0x01, 0xCA]); // INC $01 : DEX
        stub.extend_from_slice(&[0xD0, back(wl, stub.len())]);
        // APU: $4000-$4013 = 0, $4015 = $0F, $4017 = $40
        stub.extend_from_slice(&[0xA2, 0x13, 0xA9, 0x00]);
        let ap = stub.len() as u8;
        stub.extend_from_slice(&[0x9D, 0x00, 0x40, 0xCA]); // STA $4000,X : DEX
        stub.extend_from_slice(&[0x10, back(ap, stub.len())]); // BPL ap
        stub.extend_from_slice(&[0xA9, 0x0F, 0x8D, 0x15, 0x40]);
        stub.extend_from_slice(&[0xA9, 0x40, 0x8D, 0x17, 0x40]);
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

    // Данные линейно от load-адреса. В PSRAM — для банков выше первого,
    // в BRAM ядра ($0000-$7FFF) — для всего остального: оттуда SM83
    // читает байт за такт. Через PSRAM фетч не успевал на штатных
    // 4.19 МГц, и процессор исполнял мусор вместо кода.
    upload_psram(NSF_PSRAM_BASE + load as u32, &data[0x70..]);
    let body = &data[0x70..];
    let fits = body.len().min(0x8000 - load as usize);
    if body.len() > fits {
        println!("GBS: {} байт за $7FFF читаются из PSRAM (банки >1)", body.len() - fits);
    }
    // Заливка в BRAM с полной сверкой и повтором. Раньше сверка была
    // выборочной и без повтора: NSF после такой же правки для PSRAM
    // починился, а GBS нет — потому что код он читает как раз отсюда, а
    // не из PSRAM. Чтение из BRAM дешёвое, поэтому сверяем каждый байт,
    // а не через один: редкое повреждение выборка могла и пропустить.
    let mut bad = 0u32;
    for attempt in 0..3u32 {
        for (k, &b) in body[..fits].iter().enumerate() {
            chipbox_write(0x11, (load as u32 + k as u32) << 8 | b as u32);
        }
        bad = 0;
        for (k, &b) in body[..fits].iter().enumerate() {
            chipbox_write(0x29, (load as u32 + k as u32) & 0x7FFF);
            if (chipbox_read(0x29) & 0xFF) as u8 != b {
                bad = bad.saturating_add(1);
            }
        }
        if bad == 0 {
            break;
        }
        PSRAM_RETRY.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        println!("GBS: попытка {} — расхождений {bad} из {fits}, повтор", attempt + 1);
    }
    GB_ROM_BAD.store(bad.min(0xFF), core::sync::atomic::Ordering::Relaxed);
    println!("GBS: сверка BRAM — расхождений {bad} из {fits}");

    // Темп: таймер из заголовка или VBlank 59.73 Гц
    // Целочисленно (см. NSF): дробь num/den, а не f64
    let (num, den): (u64, u64) = if tac & 0x04 != 0 {
        let base: u64 = match tac & 3 {
            0 => 4096,
            1 => 262_144,
            2 => 65_536,
            _ => 16_384,
        };
        (base, 256 - tma as u64)
    } else {
        (4_194_304, 70_224) // точный vblank Game Boy — 59.727 Гц
    };
    chipbox_write(0xF, ((num << 32) / (den * CHIPBOX_CLK_HZ)) as u32);
    let play_hz = num / den;

    // Только GB в миксе
    mute_all_chips();
    chipbox_write(0xC, 64 << 8);

    let num_songs = data[0x04].max(1);
    println!("GBS: {num_songs} песен, rate {play_hz} Гц; D-pad влево/вправо — переключение");
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
        // Включаем APU до вызова INIT. Драйверы рипов рассчитывают, что
        // звук уже поднят: на настоящем Game Boy это делает загрузочное
        // ПЗУ или код игры. Часть драйверов настраивает всё сама и потому
        // играла, а те, что полагаются на готовое железо (Tetris), молчали
        // — NR52 и NR50 не писал никто.
        stub[o..o + 2].copy_from_slice(&[0x3E, 0x80]); // LD A,$80
        stub[o + 2..o + 4].copy_from_slice(&[0xE0, 0x26]); // LDH ($26),A — звук вкл
        stub[o + 4..o + 6].copy_from_slice(&[0x3E, 0xFF]); // LD A,$FF
        stub[o + 6..o + 8].copy_from_slice(&[0xE0, 0x25]); // LDH ($25),A — панорама
        stub[o + 8..o + 10].copy_from_slice(&[0x3E, 0x77]); // LD A,$77
        stub[o + 10..o + 12].copy_from_slice(&[0xE0, 0x24]); // LDH ($24),A — громкость
        o += 12;
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
        mute_all_chips();
        chipbox_write(0xC, 64 << 8);
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
    mute_all_chips();
    chipbox_write(0xC, 32 << 16);

    // vblank как дробь: NTSC 59.83 Гц, PAL 50.12 Гц
    let (vb_num, vb_den): (u64, u64) = if ntsc { (5983, 100) } else { (5012, 100) };
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
        upload_psram(NSF_PSRAM_BASE + load_c as u32, &body_vec);

        let mut stub: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        stub.push(0x78); // SEI
        stub.extend_from_slice(&[0xD8, 0xA2, 0xFF, 0x9A]); // CLD, LDX #$FF, TXS
        // Нулевая страница и стек теперь читаются из BRAM ядра, а её
        // содержимое переживает смену трека — чистим сами. Образ в PSRAM
        // обнуляется выше, но на эти две страницы он больше не влияет.
        stub.extend_from_slice(&[0xA9, 0x00, 0xAA]); // LDA #0 : TAX
        let zp = 0x0334u16 + stub.len() as u16;
        stub.extend_from_slice(&[0x9D, 0x00, 0x00]); // STA $0000,X
        stub.extend_from_slice(&[0x9D, 0x00, 0x01]); // STA $0100,X
        stub.push(0xE8); // INX
        let back = (zp as i32 - (0x0334 + stub.len() as i32 + 2)) as i8 as u8;
        stub.extend_from_slice(&[0xD0, back]); // BNE zp
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
        // Стаб и векторы — через сверяемую заливку. Раньше они писались
        // вслепую, и первое включение SID молчало, а «вправо» лечило:
        // сам образ песни сверялся, а эти три десятка байт нет.
        upload_psram(NSF_PSRAM_BASE + 0x334, &stub);
        let vecs = [
            rti_at as u8, (rti_at >> 8) as u8,
            0x34, 0x03,
            rti_at as u8, (rti_at >> 8) as u8,
        ];
        upload_psram(NSF_PSRAM_BASE + 0xFFFA, &vecs);

        // темп песни: бит в speed-маске: 1 = CIA ~60 Гц, 0 = VBlank
        let bit = speed >> core::cmp::min(s as u32, 31) & 1;
        let (num, den) = if bit != 0 { (60u64, 1u64) } else { (vb_num, vb_den) };
        chipbox_write(0xF, ((num << 32) / (den * CHIPBOX_CLK_HZ)) as u32);

        ctrl_reset(); // сброс чипов
        // гейны заново: стоп (hold) их глушит
        mute_all_chips();
        chipbox_write(0xC, 32 << 16);
        ctrl_mode(0x14); // sid_mode | cpu_run
        println!("SID: песня {} ({num}/{den} Гц)", s + 1);
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

    mute_all_chips();
    chipbox_write(0xC, 16 << 24); // только OPL3; 64 клиппинговало (как в VGM-пути)
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
    mute_all_chips();
    chipbox_write(0x15, 32u32 << 8 | 64);
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

    let is_m3u = files::has_ext(&own_path, ".m3u");
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
            let tail = files::tail(&path, 26);
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
            let tail = files::tail(&path, 24);
            let mut msg = String::from("unknown format: ");
            msg.push_str(tail);
            return error_wait("?", &msg);
        }
    };

    let (title, sub) = gd3_lines(data, &header);
    let (guessed, chips) = vgm_desc(&header.clocks);
    // Имя системы у файла обычно написано в GD3 (строка 4), и оно точнее
    // вывода по набору чипов: Pico и SG-1000 несут тот же SN76489, что и
    // Master System, и подписывались им же. Вывод по чипам оставлен
    // запасным — у части рипов тег пуст.
    let system = header
        .gd3_offset
        .and_then(|o| Gd3::parse(data, o))
        .and_then(|g| gd3_field(&g, 4))
        .unwrap_or_else(|| guessed.into());
    let draw = || {
        ui::screen("VGM", title.as_deref().unwrap_or(""), &sub, &system, &chips, None, pl.track());
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
        && header.clocks.huc6280 == 0
        && header.clocks.ym2203 == 0
        && header.clocks.ym2608 == 0
        && header.clocks.gb_dmg == 0
        // RF5C164/RF5C68 забыли внести, когда добавляли сам чип: условие
        // правилось в стенде и не правилось здесь. Рипы Mega CD, где
        // кроме него ничего нет (Sonic CD), отвергались до начала игры с
        // сообщением «нет поддержанных чипов».
        && header.clocks.rf5c164 == 0
        && header.clocks.rf5c68 == 0
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
    // Глушим всё до того, как расставим гейны этого файла: иначе чип,
    // которого в файле нет, подмешивает свой холостой уровень.
    mute_all_chips();
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
        // Разновидность шума из заголовка: маска отводов и признак
        // 15-битного регистра. У Master System 0x0009 и 16 бит, у
        // SN76489AN (SG-1000, аркады, BBC) — 0x0003 и 15.
        let sr15 = (header.clocks.sn_sr_width == 15) as u32;
        chipbox_write(0x2E, sr15 << 16 | header.clocks.sn_feedback as u32);
    }
    let scc_clk = header.clocks.k051649;
    if scc_clk != 0 {
        // Заголовок VGM несёт половину шинной частоты MSX. У эталона
        // (libvgm, k051649.c) шаг фазы считается от clock*2, и нота
        // выходит f = clock/(16*(N+1)) — вдвое выше расхожей формулы с
        // 32. Отдаём чипу полную частоту, иначе SCC играет октавой ниже.
        chipbox_write(0x21, (((scc_clk as u64 * 2) << 32) / CHIPBOX_CLK_HZ) as u32);
        chipbox_write(0x22, gain_of(header, 64, 0x19)); // scc_gain
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
    // OPN (YM2203/YM2608): своего RTL нет, но FM-часть регистрово
    // совместима с нашим YM2612, а SSG — это jt49. Делители сняты с
    // эталона (libvgm, fmopn.c): частота FM равна clock/(72*pre), SSG
    // получает clock*2/(4*pre), где pre = 1 у YM2203 и 2 у YM2608. Наш
    // jt12 считает по-YM2612 и делит на 144, поэтому в него уходит
    // clock*2/pre. Раньше в FM шёл мастер-клок, а в SSG его четверть для
    // обоих чипов — у YM2203 это давало октаву вниз на всём чипе.
    // ADPCM и ритм-часть мы не умеем: они молчат.
    let (opn_clk, opn_pre) = if header.clocks.ym2608 != 0 {
        (header.clocks.ym2608 & 0x3FFF_FFFF, 2u64)
    } else {
        (header.clocks.ym2203 & 0x3FFF_FFFF, 1u64)
    };
    if opn_clk != 0 {
        let fm = opn_clk as u64 * 2 / opn_pre;
        chipbox_write(0x16, ((fm << 32) / CHIPBOX_CLK_HZ) as u32);
        chipbox_write(4, (((opn_clk as u64 / (2 * opn_pre)) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    let okim_clk = header.clocks.okim6295 & 0x7FFF_FFFF; // бит31 = флаг чипа
    if okim_clk != 0 {
        chipbox_write(0x23, (((okim_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
        chipbox_write(0x24, 1 << 8 | gain_of(header, 64, 0x18)); // ss=1, okim_gain
    }
    let huc_clk = header.clocks.huc6280;
    if huc_clk != 0 {
        chipbox_write(0x27, (((huc_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
        // huc_gain 228, а не 128: PC Engine был тише Mega Drive на 4-8 дБ,
        // и это слышно как «тихие файлы TG-16». Прошлый заход добирал
        // недостачу на глаз («остаток ~6 дБ»), и добрал не до конца.
        //
        // Замер: тоновым сигналом MDFourier против НАСТОЯЩЕГО TG-16 наш
        // HuC6280 совпадает в пределах ±0.4 дБ до 5 кГц, то есть модель
        // чипа верна, а после снятия общего смещения кривая по полосам на
        // музыке ровная в пределах ±0.9 дБ — ничего не пропадает, разница
        // чисто в уровне. Рендеры одним трактом: Batman -21.5 дБ против
        // -13.3…-17.2 у четырёх файлов Mega Drive.
        //
        // Отношение громкостей ДВУХ РАЗНЫХ приставок по железу измерить
        // нечем: у каждой записи своё усиление тракта, а базы сняты
        // разными людьми на разной аппаратуре. Поэтому цель здесь —
        // внутренняя согласованность плеера: PC Engine вровень с Mega
        // Drive на типичных файлах. Запас есть, у Batman пик был -6 дБ.
        chipbox_write(0x28, gain_of(header, 228, 0x1B));
    }
    let k060_clk = header.clocks.k053260;
    if k060_clk != 0 {
        chipbox_write(0x25, (((k060_clk as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
        chipbox_write(0x26, gain_of(header, 64, 0x1D)); // k060_gain
    }
    // Genesis-баланс: PSG заметно тише FM. Гейн FM включает и OPN: у
    // файла YM2608/YM2203 поле клока YM2612 пустое, и раньше FM глушился
    // в ноль — маршрутизация работала, а на выходе была тишина.
    // Уровень FM: сравнение с эталоном показало отставание на 10 дБ при
    // верном балансе полос и огибающей — мы просто не добирали громкость,
    // используя меньше трети шкалы. Вернуть все 10 дБ нельзя: у самого
    // громкого из проверенных треков запас до потолка 6.5 дБ. Берём +4 дБ
    // (x1.6), это оставляет около 2.5 дБ и на нём. Отношение FM к PSG
    // сохранено — по полосам оно сходилось с эталоном.
    // Номер чипа для гейна FM и для гейна SSG/AY. У составных OPN парная
    // часть (SSG) адресуется тем же номером с битом 7 — именно там рипы и
    // задают баланс FM против SSG.
    let fm_id: u8 = if fm_clk != 0 {
        0x02
    } else if header.clocks.ym2608 != 0 {
        0x07
    } else {
        0x06
    };
    let ssg_id: u8 = if header.clocks.ym2608 != 0 {
        0x87
    } else if header.clocks.ym2203 != 0 {
        0x86
    } else {
        0x12
    };
    chipbox_write(
        0x15,
        // В файлах Mega Drive гейн PSG 36, а не 33. Относительно FM наш
        // PSG был тише НАСТОЯЩЕЙ приставки: замер тоновым сигналом
        // MDFourier (scripts/mdfourier_vgm.py) против базы записей той же
        // MDFourier — 21 запись, 19 консолей плюс два Genesis 3. Причём
        // libvgm ошибается в ДРУГУЮ сторону на 2.31 дБ, то есть подбирать
        // это соотношение по нему, как делалось раньше, значило уезжать
        // от железа вдвое дальше; разброс между двумя эмуляциями 5 дБ.
        //
        // Цель — МЕДИАНА ПАРКА MODEL 1, а не одна консоль и не все
        // подряд. Model 1 это та машина, на которой музыку для приставки
        // писали и проверяли. По одиннадцати её экземплярам отношение PSG
        // к FM расходится на 1.85 дБ (от -0.04 у японской VA1 до +1.81 у
        // MD1 VA7), медиана +0.65; 39 -> 36 снимает 0.70 дБ, остаток
        // -0.04. Первый заход целился в одну VA1 и оставлял нас горячее
        // остальных Model 1.
        //
        // Model 2 сюда не входит: там PSG микшируется горячее (медиана
        // +1.50 по семи экземплярам), и целиться в общую медиану значило
        // бы тянуть звук к более поздней и менее эталонной ревизии.
        //
        // Genesis 3 из расчёта исключён: там не YM2612, а ASIC-клон, и он
        // выпадает на 7 дБ по FM и на -7 дБ по этому отношению. Подгонять
        // под него значило бы испортить всё остальное.
        //
        // Вторая половина поправки отдана понижением FM (239 -> 207, ниже
        // по этому же вызову): поднимать сумму нельзя, у Streets of Rage 2
        // пик и без того стоял на 99.8% шкалы.
        //
        // Master System и Game Gear не тронуты: замер сделан на МИКСЕ Mega
        // Drive, где PSG складывается с YM2612, а у них тракт свой и
        // записи с железа под рукой нет.
        gain_of(header, if sn_clk != 0 { if fm_clk != 0 { 36 } else { 33 } } else { 0 }, 0x00) << 8
            // Гейн FM разный у Mega Drive и у OPN, и это следствие замера
            // (scripts/gain_ratio.py, синтетический тон на тактовой
            // железа, эталон с ТОЧНЫМ ядром Nuked — у libvgm по умолчанию
            // стоит приближение из MAME, точное выключено ради скорости).
            //
            // Mega Drive: отношение FM к PSG у эталона +9.5 дБ против
            // наших -1.6, поправка +11 дБ. Раскладывается устойчиво:
            // наш FM тише эталона на 7.4 дБ, PSG громче на 3.7. Обе части
            // и отданы по отдельности: FM x2.34 (102 -> 239), PSG /1.53
            // (51 -> 33).
            //
            // Первый заход раскладывал иначе (FM 160, PSG 23), из опасения
            // упереться в потолок. Тогда опасение сочли устаревшим по
            // замеру «пик 4538 из 32767». Тот замер был снят до
            // нескольких смен микшера и давно неверен: при 239 пик Streets
            // of Rage 2 стоит на 32704, то есть в 0.02 дБ от потолка.
            //
            // Поэтому поправку по железу (см. PSG выше) разделили пополам,
            // и половина её отдана понижением FM: 239 -> 207, минус
            // 1.25 дБ. Пик того же трека уходит к 28300, соотношение
            // PSG/FM сдвигается на измеренные 2.70 дБ.
            //
            // OPN: отношение FM к SSG у эталона +9.1 дБ против наших +3.0,
            // поправка +6.1 дБ — ровно множитель 2, которым libvgm делит
            // громкость ПАРНОЙ части составного чипа (SSG внутри OPN тише
            // отдельного AY). Отдаём подъёмом FM (102 -> 204), а не
            // понижением SSG: так же верно по отношению, но громче.
            | gain_of(header, if fm_clk != 0 { 207 } else if opn_clk != 0 { 204 } else { 0 }, fm_id),
    );
    // Гейны: неиспользуемые чипы глушим; SegaPCM 34/64 — баланс Out Run
    // по MAME (0.30 FM / 0.70 PCM с учётом нативных амплитуд ядер)
    let gains = gain_of(header, if adpcm_clk != 0 { 64 } else { 0 }, 0x17) << 24
        | gain_of(header, if pcm_clk != 0 { 34 } else { 0 }, 0x04) << 16
        // AY остаётся на 64. Поднимал до 128 по изолированному замеру
        // (msx_part.py), но синтетический тон показал, что при 128 мы на
        // 6.1 дБ ГРОМЧЕ эталона, и полный микс на корпусе это
        // подтверждает. Изоляция врёт: libvgm нормирует выход по числу
        // объявленных чипов у SCC и не нормирует у AY, и сравнивать по
        // ней абсолютные уровни нельзя.
        // SSG остаётся на 64 и для отдельного AY, и внутри OPN: перекос
        // FM против SSG выправлен подъёмом FM, см. комментарий выше.
        | gain_of(header, if ay_clk != 0 || opn_clk != 0 { 64 } else { 0 }, ssg_id) << 8
        | gain_of(header, if ym_clk != 0 { 64 } else { 0 }, 0x03);
    chipbox_write(6, gains);
    // Выходной фильтр Mega Drive (см. chipbox.sv): у консоли в тракте
    // стоит ФНЧ, и он есть у обеих моделей. Включаем только там, где это
    // действительно Mega Drive — у OPN-рипов с PC-98 тот же jt12, но
    // никакого фильтра приставки в тракте нет. 0 = Model 1, 3 = выключен.
    {
        use core::sync::atomic::Ordering::Relaxed;
        let on = fm_clk != 0;
        MD_FILTER_ON.store(on, Relaxed);
        let m = if on { md_filter_mode() } else { 3 };
        MD_FILTER_CUR.store(m, Relaxed);
        chipbox_write(0x2C, m);
    }
    // Выходной тракт NES (см. chipbox.sv): цепочка ФВЧ 90 и 440 Гц и ФНЧ
    // 14 кГц, задокументированная на NESdev. Включаем только там, где это
    // действительно Famicom.
    nes_filter_set(nes_clk != 0);
    // RF5C164 (Mega CD) и его аркадный родич RF5C68. Отсчёт чип выдаёт раз
    // в 384 такта своей тактовой; гейн предварительный, калибруется методом
    // задачи 69.
    let rf5c_clk = if header.clocks.rf5c164 != 0 {
        header.clocks.rf5c164
    } else {
        header.clocks.rf5c68
    };
    if rf5c_clk != 0 {
        chipbox_write(0x33, ((((rf5c_clk / 384) as u64) << 32) / CHIPBOX_CLK_HZ) as u32);
    }
    // Гейн 255, а не 64: модуль делит сумму восьми каналов на четыре ради
    // запаса по разрядности, и без компенсации мы тише эталона ровно на 12 дБ.
    chipbox_write(0x32, if rf5c_clk != 0 { 255 } else { 0 });
    // Гейн дисковой приставки, откалиброван по методу задачи 69:
    // синтетический файл, где сначала звучит импульсный канал APU, потом
    // волновая таблица FDS — оба в одном файле, поэтому множитель
    // нормировки эталона сокращается и сравнивается отношение чипов.
    //     громко: эталон -4.5 дБ, наше -7.3 -> поправка +2.8
    //     тише:   эталон -4.4 дБ, наше -7.2 -> поправка +2.8
    // Поправка не зависит от уровня, значит это чистый гейн, а не форма
    // таблицы громкости. 64 / 10^(2.8/20) = 46.
    chipbox_write(0x31, if header.clocks.fds { 46 } else { 0 });
    // {opl_gain, sid_gain, gb_gain, apu_gain}. У VGM-AdLib регистры громкости
    // выкручены сильнее, чем у нашего MIDI-конвертера: на 64 выход клиппит
    // (проверено на Dune в симуляции), поэтому для VGM берём 16.
    //
    // APU: 80, а не 120. На 120 громкие рипы NES упирались в полную шкалу —
    // пик самой музыки (не считая щелчка на старте) по замеру стенда с
    // ключом --apu-gain, пересчитанный на 120:
    //     Mr. Gimmick 12  +2.77 дБ FS   (предельный гейн  87)
    //     Mr. Gimmick 03  +0.75          110
    //     Mr. Gimmick 08  -0.77          131
    //     Arumana (FDS)   -2.92          168
    //     Castlevania 01  -4.08          192
    // На 80 самый громкий из них ложится на -0.74 дБ FS. Клиппинг на
    // транзиентах ударных и есть та регрессия, из-за которой семейство NES
    // стало «звучать хуже, чем в начале проекта»: до коммита af50ddc гейн
    // был 64 и в шкалу не упирался. Баланс каналов при этом исправен —
    // проверен синтетическим тоном, расхождение с эталоном 1.3 дБ.
    let opl_id: u8 = if header.clocks.ymf262 != 0 {
        0x0C
    } else if header.clocks.ym3526 != 0 {
        0x0A
    } else {
        0x09
    };
    chipbox_write(
        0xC,
        gain_of(header, if opl_clk != 0 { 16 } else { 0 }, opl_id) << 24
            | gain_of(header, if nes_clk != 0 { 80 } else { 0 }, 0x14)
            | gain_of(header, if header.clocks.gb_dmg != 0 { 64 } else { 0 }, 0x13) << 8,
    );
    ctrl_reset();
    // Game Boy в VGM: рипа с кодом нет, играет поток записей в APU.
    // Бит 8 выводит звуковую часть из сброса, не поднимая SM83. Строго
    // после ctrl_reset: тот обнуляет слово режима целиком.
    if header.clocks.gb_dmg != 0 {
        ctrl_mode(1 << 8);
    }

    let mut sink = CmdSink::new();
    // SCC: разблокировать регистры звука (BR2=0x3F) первой командой FIFO
    if scc_clk != 0 {
        sink.push(OP_EXT | EXT_SCC | (7u32 << 16));
    }
    // Game Boy: привести APU в то состояние, в котором его оставляет
    // загрузчик приставки.
    //
    // Запись в звуковые регистры игнорируется, пока не поднят бит 7 NR52
    // — так устроено и железо. Но на настоящем Game Boy звук включает
    // загрузчик ещё до старта игры, поэтому рип, который сам NR52 не
    // пишет, на приставке играет, а у нас молчал целиком. Ровно то же
    // правило, на котором уже молчали GBS и NSF: рип рассчитывает на
    // готовое железо и поднимать его не обязан.
    //
    // Значения — задокументированное состояние DMG после загрузчика:
    // NR52 = 0x80 (питание), NR50 = 0x77 (обе стороны на максимум),
    // NR51 = 0xF3 (все каналы влево, первые два ещё и вправо). Если файл
    // пишет их сам, наши просто перекроются.
    if header.clocks.gb_dmg != 0 {
        for (reg, val) in [(0x26u32, 0x80u32), (0x24, 0x77), (0x25, 0xF3)] {
            sink.push(OP_EXT | EXT_GB | reg << 8 | val);
        }
    }

    // NES: то же правило и по той же причине.
    //
    // $4015 разрешает каналы, и по включению он нулевой — каналы молчат.
    // Драйвер игры пишет его при своей инициализации, но у части рипов
    // запись осталась ЗА кадром лога: с устройства пришло «таймер идёт,
    // музыки нет» по Castlevania, Excitebike, Battletoads, Zelda и
    // Metroid. Проверка по корпусу: у обоих молчащих файлов записи $4015
    // нет вовсе, у обоих играющих — есть (TMNT III пишет 0F, Arumana 0B).
    //
    // Эталон делает ровно это: в libvgm (np_nes_apu.c, np_nes_dmc.c) есть
    // UNMUTE_ON_RESET, по сбросу пишущая $4015 = 0x0F, и она включена по
    // умолчанию. Файл, который пишет регистр сам, просто перекроет наше.
    if header.clocks.nes_apu != 0 {
        sink.push(OP_APU | 0x15 << 8 | 0x0F);
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
    // Затухание каждого канала по двум сторонам T6W28 (Neo Geo Pocket).
    // У нас один jt89, поэтому стороны сводятся в моно по громкой:
    // 0 — полная громкость, 15 — тишина, значит берём минимум. Простое
    // «играть только сторону 0» потеряло бы голоса, отведённые вправо.
    let mut sn_att = [[15u8; 4]; 2];
    let sn_dual = header.clocks.sn_dual;
    // Маска стерео Game Gear: биты 0-3 — правая сторона каналов 0..3,
    // биты 4-7 — левая. 0xFF означает «всё в оба уха».
    let mut gg_mask: u8 = 0xFF;
    // Куда ляжет следующий байт ОЗУ RF5C164, если указатель не разрывать
    let mut rf5c_ptr: u16 = 0xFFFF;
    // Банк сэмплов RF5C: блоки типа 0x01/0x02 лежат в файле целиком, а
    // команда 0x68 копирует из них куски в ОЗУ чипа по ходу трека.
    let mut rf5c_bank: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    // Окно записи в ОЗУ — 4 КБ, страницу задаёт регистр 0x07 при сброшенном
    // бите 6. Оно накладывается на ВСЕ пути записи: команды 0xC1/0xC2, блоки
    // 0xC0/0xC1 и заливку 0x68 (libvgm, DoRAMOfsPatches). Без него рип
    // Sonic CD лил все сэмплы в первые 4 КБ поверх друг друга.
    let mut rf5c_wbank: u32 = 0;
    // Стерео включаем только там, где файл его объявил: у T6W28 (Neo Geo
    // Pocket) или у файла с маской Game Gear. Обычные моно-файлы идут
    // прежней дорогой через sn_sound и звучат бит в бит как раньше.
    // T6W28 объявлен в заголовке, а Game Gear узнаётся только по первой
    // маске стерео в потоке — там путь включается по факту её прихода.
    let mut sn_stereo = header.clocks.sn_t6w28;
    chipbox_write(0x2F, sn_stereo as u32);
    // Отброшенное за проход: записи второго экземпляра чипа и маски
    // стерео Game Gear. И то, и другое раньше уходило в тишину без
    // сообщения (а маска ещё и глушила канал шума, потому что попадала в
    // PSG как данные). Считаем и говорим один раз в конце прохода.
    // Счётчики видны и на экране, если включён Developer info: до этого
    // они жили только в выводе стенда, и на устройстве понять, что часть
    // записей отбрасывается, было нечем.
    DROP2.store(0, core::sync::atomic::Ordering::Relaxed);
    DROP_GG.store(0, core::sync::atomic::Ordering::Relaxed);

    loop {
        match reader.next_event() {
            Ok(Event::Write { chip: Chip::Ym2151, addr, data, .. }) => {
                sink.push(OP_YM2151 | (addr as u32) << 8 | data as u32);
            }
            Ok(Event::Write { chip: Chip::Ym2612, port, addr, data }) => {
                sink.push(OP_FM2612 | (port as u32) << 16 | (addr as u32) << 8 | data as u32);
            }
            Ok(Event::Write { chip: Chip::Sn76489, port, data, .. }) => {
                // Защёлка громкости: бит 7 задаёт регистр, бит 4 отличает
                // громкость от тона. Частоты, вторые байты данных и режим
                // шума пишет только сторона 0 — их шлём как есть.
                if data & 0x90 == 0x90 {
                    let ch = (data >> 5 & 3) as usize;
                    // У T6W28 стороны пишутся раздельно (порт 0 и 1); у
                    // обычного чипа сторона одна и та же для обеих.
                    if sn_dual {
                        sn_att[port as usize & 1][ch] = data & 0x0F;
                    } else {
                        sn_att[0][ch] = data & 0x0F;
                        sn_att[1][ch] = data & 0x0F;
                    }
                    // В сам чип по-прежнему уходит сторона 0: моно-путь
                    // остаётся рабочим, если стерео не включено.
                    if port == 0 || !sn_dual {
                        sink.push(OP_SN | 0x90 | (ch as u32) << 5 | (data & 0x0F) as u32);
                    }
                    // Только при включённом стерео: иначе моно-файлы
                    // получили бы вдвое больше команд в очереди на ровном месте.
                    if sn_stereo {
                        sn_push_att(&mut sink, &sn_att, gg_mask);
                    }
                } else if port == 0 {
                    sink.push(OP_SN | data as u32);
                }
            }
            // Маска стерео Game Gear: биты 0-3 — правая сторона каналов
            // 0..3, биты 4-7 — левая. Выключенный канал глушим на своей
            // стороне аттенюацией 15, включённый берёт свою громкость.
            Ok(Event::GgStereo { chip2: false, mask }) => {
                gg_mask = mask;
                if !sn_stereo {
                    sn_stereo = true;
                    chipbox_write(0x2F, 1);
                }
                sn_push_att(&mut sink, &sn_att, mask);
            }
            Ok(Event::Write { chip: Chip::Opl, port, addr, data }) => {
                // OPL2/OPL3 играем на нашем OPL3: port = банк регистров
                sink.push(OP_OPL3 | (port as u32) << 16 | (addr as u32) << 8 | data as u32);
            }
            Ok(Event::Write { chip: Chip::GbDmg, addr, data, .. }) => {
                // Регистры APU Game Boy $FF10-$FF3F: в VGM адрес идёт
                // смещением от $FF10, чипу нужен полный младший байт.
                sink.push(OP_EXT | EXT_GB | ((addr as u32) + 0x10) << 8 | data as u32);
            }
            Ok(Event::Write { chip: Chip::Rf5c164, addr, data, .. }) => {
                if addr & 0x7F == 0x07 && data & 0x40 == 0 {
                    rf5c_wbank = (data & 0x0F) as u32;
                }
                sink.push(OP_EXT | EXT_RF5C | (addr as u32 & 0xF) << 8 | data as u32);
            }
            // Байт в ОЗУ сэмплов. Указатель шлём только когда он разорван:
            // рипы пишут длинными подряд идущими кусками, и слать пару на
            // каждый байт значило бы удвоить очередь на ровном месте.
            Ok(Event::Rf5cMem { offset, data: d }) => {
                let offset = (offset & 0x0FFF) | (rf5c_wbank as u16) << 12;
                if offset != rf5c_ptr {
                    sink.push(OP_EXT | EXT_RF5C_PTR | offset as u32);
                }
                sink.push(OP_EXT | EXT_RF5C_RAM | d as u32);
                rf5c_ptr = offset.wrapping_add(1);
            }
            Ok(Event::DataBlock { kind: 0x01 | 0x02, start, len }) => {
                rf5c_bank.extend_from_slice(&data[start..start + len]);
            }
            // Заливка из банка: у Sonic CD это единственный путь сэмплов
            Ok(Event::PcmRamWrite { kind: 0x01 | 0x02, src, dst, len }) => {
                // Эталон вылезающую за банк заливку не обрезает, а игнорирует
                let from = src as usize;
                let n = len as usize;
                if from < rf5c_bank.len() && n <= rf5c_bank.len() - from {
                    let dst = (dst | rf5c_wbank << 12) & 0xFFFF;
                    sink.push(OP_EXT | EXT_RF5C_PTR | dst);
                    for &b in &rf5c_bank[from..from + n] {
                        sink.push(OP_EXT | EXT_RF5C_RAM | b as u32);
                    }
                    rf5c_ptr = (dst as u16).wrapping_add(n as u16);
                }
            }
            // Дамп ОЗУ блоком: смещение 16 бит, дальше тело
            Ok(Event::DataBlock { kind: 0xC0 | 0xC1, start, len }) if len >= 2 => {
                let block = &data[start..start + len];
                let a = (block[0] as u32 | (block[1] as u32) << 8 | rf5c_wbank << 12) & 0xFFFF;
                sink.push(OP_EXT | EXT_RF5C_PTR | a);
                for &b in &block[2..] {
                    sink.push(OP_EXT | EXT_RF5C_RAM | b as u32);
                }
                rf5c_ptr = (a as u16).wrapping_add(len as u16 - 2);
            }
            Ok(Event::Write { chip: Chip::HuC6280, addr, data, .. }) => {
                sink.push(OP_EXT | EXT_HUC | ((addr & 0xF) as u32) << 8 | data as u32);
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
            Ok(Event::Write { chip: Chip::Opn, port, addr, data }) => {
                if port == 0 && addr < 0x10 {
                    sink.push(OP_AY | ((addr & 0xF) as u32) << 8 | data as u32);
                } else if addr >= 0x20 && !(port == 0 && (0x2D..=0x2F).contains(&addr)) {
                    // $10-$1F порта 0 — ритм, низ порта 1 — ADPCM-B:
                    // и то и другое пропускаем. $2D-$2F — прескейлер,
                    // jt12 его не знает, а частоту мы задали снаружи.
                    sink.push(OP_FM2612 | (port as u32) << 16 | (addr as u32) << 8 | data as u32);
                }
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
            // Дисковая приставка Famicom адресуется тем же кодом команды.
            // Пересчёт адресов взят с libvgm (Cmd_NES_Reg): 0x3F — это
            // общее разрешение ввода-вывода $4023, 0x20-0x3E ложатся на
            // $4080-$409E, а 0x40-0x7F проходят как есть и попадают в
            // волновую таблицу $4040-$407F.
            Ok(Event::Write { chip: Chip::NesApu, addr, data: d, .. }) => {
                let reg = if addr == 0x3F {
                    0x23u32
                } else if addr & 0xE0 == 0x20 {
                    0x80 | (addr as u32 & 0x1F)
                } else {
                    addr as u32
                };
                sink.push(OP_EXT | EXT_FDS | reg << 8 | d as u32);
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
                    let mut fbuf = [0u8; 16];
                    ui::progress(el.min(total_s), total_s, diag_ff(&mut fbuf));
                }
            }
            // Отброшенное осознанно. Ветки нужны явные: ниже стоит
            // «Ok(_) => {}», и молчаливое проглатывание этих двух команд
            // как раз и было дефектом.
            Ok(Event::SecondChip { .. }) => bump(&DROP2),
            // Маска второго чипа: играть её нечем, считаем как отброшенное
            Ok(Event::GgStereo { .. }) => bump(&DROP_GG),
            Ok(Event::End) => {
                loops += 1;
                let (d2, dgg) = (
                    DROP2.load(core::sync::atomic::Ordering::Relaxed),
                    DROP_GG.load(core::sync::atomic::Ordering::Relaxed),
                );
                if loops == 1 && (d2 != 0 || dgg != 0) {
                    println!("Отброшено: второй чип {d2}, стерео Game Gear {dgg}");
                }
                // Ни одной записи в чипы за весь проход: у файла всё
                // звучание приходится на то, чего мы не умеем (звук
                // дисковой приставки Famicom, PWM у 32X, WonderSwan).
                // Молча крутить таймер над тишиной — выглядит как
                // поломка загрузки; лучше сказать прямо.
                if sink.pushed == 0 {
                    println!("В потоке нет записей в поддержанные чипы");
                    return error_wait("VGM", "no data for supported chips");
                }
                if loops >= 2 || header.loop_offset.is_none() {
                    // Дать хвосту FIFO дозвучать. Время здесь обязано
                    // идти: короткий трек уходит в FIFO целиком за
                    // миллисекунды, и почти всё звучание проходит внутри
                    // этого цикла — раньше счётчик и полоса в нём стояли,
                    // хотя музыка играла.
                    while chipbox_status() & 0x1FFF != 0 {
                        match transport(&mut sink.btn, 0, pl) {
                            Some(Ctl::Redraw) => draw(),
                            Some(ctl) => return ctl,
                            None => {}
                        }
                        vu_tick(&mut vu_last);
                        let el = elapsed_s();
                        if el != shown_s {
                            shown_s = el;
                            let mut fbuf = [0u8; 16];
                            ui::progress(el.min(total_s), total_s, diag_ff(&mut fbuf));
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
