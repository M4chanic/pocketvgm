//! Прогон парсера по файлу на хосте — как это делает фирмварь.
//!
//! Нужен, чтобы ловить падения на конкретных рипах без устройства: на
//! экране плеера видно только обрезанный текст паники, а здесь тот же
//! код падает с полным сообщением и местом.
//!
//! Использование: cargo run --example scan_file -- файл.vgm|.vgz [...]

use std::io::Read;

fn main() {
    let mut bad = 0;
    for path in std::env::args().skip(1) {
        let mut raw = Vec::new();
        std::fs::File::open(&path)
            .expect("не открыть")
            .read_to_end(&mut raw)
            .expect("не прочитать");
        let data = if raw.len() >= 2 && raw[0..2] == vgm_core::GZIP_MAGIC {
            match vgm_core::decompress(&raw) {
                Ok(v) => v,
                Err(_) => {
                    println!("{path}: РАСПАКОВКА НЕ УДАЛАСЬ");
                    bad += 1;
                    continue;
                }
            }
        } else {
            raw
        };
        let header = match vgm_core::Header::parse(&data) {
            Ok(h) => h,
            Err(e) => {
                println!("{path}: ЗАГОЛОВОК НЕ РАЗОБРАН: {e:?}");
                bad += 1;
                continue;
            }
        };
        let mut reader = vgm_core::Reader::new(&data, header.data_offset);
        let mut events = 0u64;
        let mut waits = 0u64;
        loop {
            match reader.next_event() {
                Ok(vgm_core::Event::End) => break,
                Ok(vgm_core::Event::Wait { ticks }) => {
                    waits += ticks as u64;
                    events += 1;
                }
                Ok(_) => events += 1,
                Err(e) => {
                    println!("{path}: ОШИБКА ПОТОКА после {events} команд: {e:?}");
                    bad += 1;
                    break;
                }
            }
            if events > 200_000_000 {
                println!("{path}: поток не кончается, оборвано");
                bad += 1;
                break;
            }
        }
        println!(
            "{}: {} байт, команд {}, время {:.1} с",
            path.rsplit('/').next().unwrap_or(&path),
            data.len(),
            events,
            waits as f64 / 44100.0
        );
    }
    if bad > 0 {
        std::process::exit(1);
    }
}
