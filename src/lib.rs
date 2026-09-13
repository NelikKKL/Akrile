//! akrile — собственный архивный формат (.akrile) и его WASM-ядро.
//!
//! Формат файла:
//!
//! [ HEADER ]
//!   magic:        4 bytes  = "AKRL"
//!   version:      1 byte   = 1
//!   flags:        1 byte   = 0 (зарезервировано)
//!   entry_count:  4 bytes  LE u32
//!
//! [ ENTRY DATA ]  (по одному на файл, последовательно)
//!   name_len:      2 bytes LE u16
//!   name:          UTF-8, name_len байт
//!   method:        1 byte  (0 = store, 1 = deflate)
//!   crc32:         4 bytes LE u32   (crc исходных, несжатых данных)
//!   uncompressed:  8 bytes LE u64
//!   compressed:    8 bytes LE u64
//!   data:          compressed байт
//!
//! [ CENTRAL DIRECTORY ]  (индекс, как в zip, для произвольного доступа)
//!   на каждую запись: name_len, name, method, crc32, uncompressed,
//!   compressed, offset (8 bytes LE u64 — смещение до ENTRY DATA)
//!
//! [ FOOTER ]
//!   central_dir_offset: 8 bytes LE u64
//!   central_dir_count:  4 bytes LE u32
//!   end_magic:          4 bytes = "AKRE"
//!
//! Архитектурно код разделён на два слоя:
//!   - "чистая" логика (структуры Entry/AkrileArchive и их inherent-методы
//!     generate_bytes/parse_bytes/get_bytes) — не зависит от wasm-bindgen,
//!     компилируется и тестируется на обычном хосте (`cargo test`).
//!   - тонкий wasm-bindgen слой (#[wasm_bindgen] impl AkrileArchive) —
//!     только конвертация типов на границе с JS.

use miniz_oxide::deflate::compress_to_vec;
use miniz_oxide::inflate::decompress_to_vec;
use wasm_bindgen::prelude::*;

const MAGIC: &[u8; 4] = b"AKRL";
const END_MAGIC: &[u8; 4] = b"AKRE";
const VERSION: u8 = 1;
const DEFAULT_LEVEL: u8 = 6;

/// Минимальный размер валидного файла: header(10) + footer(16), 0 записей.
const MIN_FILE_LEN: usize = 10 + 16;

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    method: u8, // 0 = store, 1 = deflate
    crc32: u32,
    usize_: u64,
    csize: u64,
    data: Vec<u8>, // уже в сжатом (или сыром, если store) виде
}

#[wasm_bindgen]
#[derive(Debug)]
pub struct AkrileArchive {
    entries: Vec<Entry>,
}

// ---------------------------------------------------------------------
// Чистая логика без wasm-bindgen — тестируется через `cargo test --lib`.
// ---------------------------------------------------------------------
impl AkrileArchive {
    fn set_file(&mut self, name: String, data: &[u8], store_only: bool) {
        self.entries.retain(|e| e.name != name);
        let crc = crc32(data);

        if !store_only {
            let compressed = compress_to_vec(data, DEFAULT_LEVEL);
            if !compressed.is_empty() && compressed.len() < data.len() {
                self.entries.push(Entry {
                    name,
                    method: 1,
                    crc32: crc,
                    usize_: data.len() as u64,
                    csize: compressed.len() as u64,
                    data: compressed,
                });
                return;
            }
        }

        self.entries.push(Entry {
            name,
            method: 0,
            crc32: crc,
            usize_: data.len() as u64,
            csize: data.len() as u64,
            data: data.to_vec(),
        });
    }

    fn get_bytes(&self, name: &str) -> Option<Vec<u8>> {
        let e = self.entries.iter().find(|e| e.name == name)?;
        if e.method == 0 {
            Some(e.data.clone())
        } else {
            decompress_to_vec(&e.data).ok()
        }
    }

    fn generate_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(0);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());

        let mut central = Vec::new();
        for e in &self.entries {
            let offset = out.len() as u64;
            let name_bytes = e.name.as_bytes();

            out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(name_bytes);
            out.push(e.method);
            out.extend_from_slice(&e.crc32.to_le_bytes());
            out.extend_from_slice(&e.usize_.to_le_bytes());
            out.extend_from_slice(&e.csize.to_le_bytes());
            out.extend_from_slice(&e.data);

            central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            central.extend_from_slice(name_bytes);
            central.push(e.method);
            central.extend_from_slice(&e.crc32.to_le_bytes());
            central.extend_from_slice(&e.usize_.to_le_bytes());
            central.extend_from_slice(&e.csize.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
        }

        let central_dir_offset = out.len() as u64;
        out.extend_from_slice(&central);
        out.extend_from_slice(&central_dir_offset.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        out.extend_from_slice(END_MAGIC);
        out
    }

    /// Разбор .akrile из байт. Все смещения/длины из файла проверяются
    /// перед использованием — повреждённый или враждебный файл возвращает
    /// Err, а не паникует (что уронило бы весь wasm-модуль).
    fn parse_bytes(data: &[u8]) -> Result<AkrileArchive, String> {
        if data.len() < MIN_FILE_LEN {
            return Err("Invalid .akrile file: too short".into());
        }
        if &data[0..4] != MAGIC {
            return Err("Invalid .akrile file: bad magic".into());
        }
        let end = data.len();
        if &data[end - 4..end] != END_MAGIC {
            return Err("Invalid .akrile file: bad footer magic".into());
        }

        let central_count = read_u32(data, end - 8)?;
        let central_offset = read_u64(data, end - 16)? as usize;

        if central_offset > end {
            return Err("Invalid .akrile file: central directory offset out of bounds".into());
        }

        let mut entries = Vec::with_capacity(central_count as usize);
        let mut pos = central_offset;

        for _ in 0..central_count {
            let name_len = read_u16(data, pos)? as usize;
            pos = pos
                .checked_add(2)
                .ok_or("Invalid .akrile file: offset overflow")?;

            let name_bytes = slice_checked(data, pos, name_len)?;
            let name = String::from_utf8(name_bytes.to_vec())
                .map_err(|_| "Invalid .akrile file: bad utf8 name".to_string())?;
            pos += name_len;

            let method = *data.get(pos).ok_or("Invalid .akrile file: truncated entry")?;
            pos += 1;
            let crc = read_u32(data, pos)?;
            pos += 4;
            let usize_ = read_u64(data, pos)?;
            pos += 8;
            let csize = read_u64(data, pos)?;
            pos += 8;
            let offset = read_u64(data, pos)? as usize;
            pos += 8;

            // Прочитать данные записи из ENTRY DATA по offset, с проверкой границ.
            let mut lp = offset;
            let lname_len = read_u16(data, lp)? as usize;
            lp = lp
                .checked_add(2 + lname_len + 1 + 4 + 8 + 8)
                .ok_or("Invalid .akrile file: offset overflow")?;
            let fdata = slice_checked(data, lp, csize as usize)?.to_vec();

            entries.push(Entry { name, method, crc32: crc, usize_, csize, data: fdata });
        }

        Ok(AkrileArchive { entries })
    }
}

fn slice_checked(data: &[u8], start: usize, len: usize) -> Result<&[u8], String> {
    let end = start
        .checked_add(len)
        .ok_or("Invalid .akrile file: length overflow")?;
    data.get(start..end)
        .ok_or("Invalid .akrile file: truncated data".to_string())
}

fn read_u16(d: &[u8], p: usize) -> Result<u16, String> {
    let s = slice_checked(d, p, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}
fn read_u32(d: &[u8], p: usize) -> Result<u32, String> {
    let s = slice_checked(d, p, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn read_u64(d: &[u8], p: usize) -> Result<u64, String> {
    let s = slice_checked(d, p, 8)?;
    let mut b = [0u8; 8];
    b.copy_from_slice(s);
    Ok(u64::from_le_bytes(b))
}

/// CRC32 (полином 0xEDB88320, как в zip/png), без внешних зависимостей.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

impl Default for AkrileArchive {
    fn default() -> Self {
        Self::new_inner()
    }
}

impl AkrileArchive {
    fn new_inner() -> Self {
        AkrileArchive { entries: Vec::new() }
    }
}

// ---------------------------------------------------------------------
// wasm-bindgen слой: только конвертация типов на границе с JS.
// ---------------------------------------------------------------------
#[wasm_bindgen]
impl AkrileArchive {
    #[wasm_bindgen(constructor)]
    pub fn new() -> AkrileArchive {
        AkrileArchive::new_inner()
    }

    /// Добавить/заменить файл. store_only=true отключает сжатие (как STORE в zip).
    #[wasm_bindgen(js_name = addFile)]
    pub fn add_file(&mut self, name: String, data: &[u8], store_only: bool) {
        self.set_file(name, data, store_only);
    }

    #[wasm_bindgen(js_name = removeFile)]
    pub fn remove_file(&mut self, name: &str) {
        self.entries.retain(|e| e.name != name);
    }

    #[wasm_bindgen(js_name = fileNames)]
    pub fn file_names(&self) -> Vec<JsValue> {
        self.entries.iter().map(|e| JsValue::from_str(&e.name)).collect()
    }

    /// Достать распакованные данные файла по имени.
    /// Возвращает `undefined` в JS, если файла нет — явный JsValue вместо
    /// Option<Vec<u8>>, чтобы не полагаться на неявную поддержку Option<T>
    /// в wasm-bindgen для типов, отображаемых в typed array.
    #[wasm_bindgen(js_name = getFile)]
    pub fn get_file(&self, name: &str) -> JsValue {
        match self.get_bytes(name) {
            Some(bytes) => js_sys::Uint8Array::from(bytes.as_slice()).into(),
            None => JsValue::UNDEFINED,
        }
    }

    /// Сериализовать архив в байты формата .akrile.
    pub fn generate(&self) -> Vec<u8> {
        self.generate_bytes()
    }

    /// Распарсить существующий .akrile файл.
    pub fn load(data: &[u8]) -> Result<AkrileArchive, JsValue> {
        AkrileArchive::parse_bytes(data).map_err(|e| JsValue::from_str(&e))
    }
}

// ---------------------------------------------------------------------
// Юнит-тесты чистой логики (запускаются нативно: `cargo test --lib`).
// ---------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_store_and_deflate() {
        let mut a = AkrileArchive::new_inner();
        a.set_file("hello.txt".into(), b"Hello, akrile world!", false);
        a.set_file("raw.bin".into(), &[1, 2, 3, 4, 5], true);
        a.set_file("empty.txt".into(), b"", false);

        let bytes = a.generate_bytes();
        let loaded = AkrileArchive::parse_bytes(&bytes).expect("should parse");

        assert_eq!(loaded.get_bytes("hello.txt").unwrap(), b"Hello, akrile world!");
        assert_eq!(loaded.get_bytes("raw.bin").unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(loaded.get_bytes("empty.txt").unwrap(), Vec::<u8>::new());
        assert!(loaded.get_bytes("missing.txt").is_none());
    }

    #[test]
    fn overwrite_replaces_previous_entry() {
        let mut a = AkrileArchive::new_inner();
        a.set_file("a.txt".into(), b"first", false);
        a.set_file("a.txt".into(), b"second", false);
        assert_eq!(a.entries.len(), 1);

        let bytes = a.generate_bytes();
        let loaded = AkrileArchive::parse_bytes(&bytes).unwrap();
        assert_eq!(loaded.get_bytes("a.txt").unwrap(), b"second");
    }

    #[test]
    fn remove_file_removes_entry() {
        let mut a = AkrileArchive::new_inner();
        a.set_file("a.txt".into(), b"data", false);
        a.entries.retain(|e| e.name != "a.txt");
        assert!(a.entries.is_empty());
    }

    #[test]
    fn empty_archive_round_trip() {
        let a = AkrileArchive::new_inner();
        let bytes = a.generate_bytes();
        let loaded = AkrileArchive::parse_bytes(&bytes).unwrap();
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn rejects_bad_magic() {
        let err = AkrileArchive::parse_bytes(b"not an akrile file at all!!").unwrap_err();
        assert!(err.contains("bad magic"));
    }

    #[test]
    fn rejects_truncated_file() {
        let mut a = AkrileArchive::new_inner();
        a.set_file("a.txt".into(), b"some data here", false);
        let mut bytes = a.generate_bytes();
        bytes.truncate(bytes.len() / 2); // обрезаем — не должно паниковать
        assert!(AkrileArchive::parse_bytes(&bytes).is_err());
    }

    #[test]
    fn rejects_corrupted_offsets_without_panicking() {
        let mut a = AkrileArchive::new_inner();
        a.set_file("a.txt".into(), b"some data here", false);
        let mut bytes = a.generate_bytes();
        // Портим central_dir_offset в футере огромным значением.
        let len = bytes.len();
        bytes[len - 16..len - 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(AkrileArchive::parse_bytes(&bytes).is_err());
    }

    #[test]
    fn compression_actually_shrinks_repetitive_data() {
        let mut a = AkrileArchive::new_inner();
        let data = vec![b'x'; 10_000];
        a.set_file("big.txt".into(), &data, false);
        let bytes = a.generate_bytes();
        // Заголовок+футер невелики, поэтому сжатый архив должен быть
        // значительно меньше 10 000 байт исходных повторяющихся данных.
        assert!(bytes.len() < data.len() / 2);
    }
}
