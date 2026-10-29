//! akrile — собственный архивный формат для веба (ядро на Rust → WASM).
//!
//! Раскладка файла (little-endian):
//!
//! ```text
//! HEADER   : "AKRL"(4) version(u16) reserved(u32)
//! ENTRIES  : [local header + данные] * N
//!            local: name_len(u16) name method(u8) crc32(u32) usize(u64) csize(u64) data
//! CENTRAL  : [name_len(u16) name method(u8) crc32(u32) usize(u64) csize(u64) offset(u64)] * N
//! FOOTER   : central_offset(u64) count(u32) "KRLA"(4)
//! ```
//!
//! method: 0 = STORE, 1 = DEFLATE (raw).

use miniz_oxide::deflate::compress_to_vec;
use miniz_oxide::inflate::decompress_to_vec_with_limit;
use std::borrow::Cow;
use std::collections::HashMap;
use wasm_bindgen::prelude::*;

const MAGIC: &[u8; 4] = b"AKRL";
const END_MAGIC: &[u8; 4] = b"KRLA";
const VERSION: u16 = 1;

/// Уровень сжатия по умолчанию (0..=10, как в miniz).
pub const DEFAULT_LEVEL: u8 = 6;

const HEADER_LEN: usize = 4 + 2 + 4;
const FOOTER_LEN: usize = 8 + 4 + 4;
const MIN_FILE_LEN: usize = HEADER_LEN + FOOTER_LEN;
/// local header без имени и данных: name_len + method + crc + usize + csize
const LOCAL_FIXED_LEN: usize = 2 + 1 + 4 + 8 + 8;
/// минимальная запись central directory (с пустым именем)
const MIN_CENTRAL_RECORD_LEN: usize = 2 + 1 + 4 + 8 + 8 + 8;
/// теоретический максимум степени сжатия deflate (~1032:1)
const MAX_DEFLATE_RATIO: u64 = 1032;

/// Расширения уже сжатых форматов: deflate на них только тратит время.
const PRECOMPRESSED_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "mp3", "ogg", "opus", "aac", "m4a", "mp4",
    "webm", "mkv", "woff2", "zip", "gz", "br", "zst", "7z", "xz", "bz2",
];

fn is_precompressed(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, ext)) => PRECOMPRESSED_EXTS
            .iter()
            .any(|e| ext.eq_ignore_ascii_case(e)),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// CRC32 (IEEE), slicing-by-8. Таблицы строятся на этапе компиляции.
// ---------------------------------------------------------------------------

const fn make_crc_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut j = 1;
        while j < 8 {
            let prev = t[j - 1][i];
            t[j][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            j += 1;
        }
        i += 1;
    }
    t
}

static CRC_TABLES: [[u32; 256]; 8] = make_crc_tables();

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = CRC_TABLES[7][(lo & 0xFF) as usize]
            ^ CRC_TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ CRC_TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ CRC_TABLES[4][(lo >> 24) as usize]
            ^ CRC_TABLES[3][(hi & 0xFF) as usize]
            ^ CRC_TABLES[2][((hi >> 8) & 0xFF) as usize]
            ^ CRC_TABLES[1][((hi >> 16) & 0xFF) as usize]
            ^ CRC_TABLES[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = (crc >> 8) ^ CRC_TABLES[0][((crc ^ u32::from(b)) & 0xFF) as usize];
    }
    !crc
}

// ---------------------------------------------------------------------------
// Безопасное чтение
// ---------------------------------------------------------------------------

fn slice_checked(data: &[u8], offset: usize, len: usize) -> Result<&[u8], String> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| "Invalid .akrile file: offset overflow".to_string())?;
    data.get(offset..end)
        .ok_or_else(|| "Invalid .akrile file: truncated data".to_string())
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16, String> {
    let s = slice_checked(data, offset, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, String> {
    let s = slice_checked(data, offset, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64, String> {
    let s = slice_checked(data, offset, 8)?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn to_usize(v: u64) -> Result<usize, String> {
    usize::try_from(v).map_err(|_| "Invalid .akrile file: value out of range".to_string())
}

// ---------------------------------------------------------------------------
// Модель данных
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Payload {
    /// Данные добавлены через addFile — лежат в собственном буфере.
    Owned(Vec<u8>),
    /// Данные загруженного архива — только диапазон в `source`, без копии.
    Source { start: usize, len: usize },
}

#[derive(Debug)]
struct Entry {
    name: String,
    method: u8,
    crc32: u32,
    usize_: u64,
    csize: u64,
    payload: Payload,
}

#[wasm_bindgen]
#[derive(Debug)]
pub struct AkrileArchive {
    entries: Vec<Entry>,
    /// имя → позиция в `entries`
    index: HashMap<String, usize>,
    /// исходные байты загруженного архива (один раз, без копий на каждую запись)
    source: Vec<u8>,
    verify_crc: bool,
}

impl Default for AkrileArchive {
    fn default() -> Self {
        Self::new_inner()
    }
}

impl AkrileArchive {
    fn new_inner() -> Self {
        AkrileArchive {
            entries: Vec::new(),
            index: HashMap::new(),
            source: Vec::new(),
            verify_crc: false,
        }
    }

    fn insert_entry(&mut self, e: Entry) {
        if let Some(&i) = self.index.get(&e.name) {
            self.entries[i] = e;
        } else {
            self.index.insert(e.name.clone(), self.entries.len());
            self.entries.push(e);
        }
    }

    fn remove_entry(&mut self, name: &str) {
        if let Some(i) = self.index.remove(name) {
            self.entries.remove(i);
            for (j, e) in self.entries.iter().enumerate().skip(i) {
                if let Some(slot) = self.index.get_mut(e.name.as_str()) {
                    *slot = j;
                }
            }
        }
    }

    fn find(&self, name: &str) -> Option<&Entry> {
        self.index.get(name).map(|&i| &self.entries[i])
    }

    fn payload(&self, e: &Entry) -> &[u8] {
        match &e.payload {
            Payload::Owned(v) => v.as_slice(),
            // диапазон проверен в parse_entries, `source` после загрузки не меняется
            Payload::Source { start, len } => &self.source[*start..*start + *len],
        }
    }

    fn set_file(
        &mut self,
        name: String,
        data: Vec<u8>,
        store_only: bool,
        level: u8,
    ) -> Result<(), String> {
        if name.len() > usize::from(u16::MAX) {
            return Err("File name is too long (max 65535 bytes)".to_string());
        }
        let crc = crc32(&data);
        let usize_ = data.len() as u64;

        if !store_only && !is_precompressed(&name) {
            let compressed = compress_to_vec(&data, level.min(10));
            // сжатие должно реально уменьшать размер
            if !compressed.is_empty() && compressed.len() < data.len() {
                self.insert_entry(Entry {
                    name,
                    method: 1,
                    crc32: crc,
                    usize_,
                    csize: compressed.len() as u64,
                    payload: Payload::Owned(compressed),
                });
                return Ok(());
            }
        }

        self.insert_entry(Entry {
            name,
            method: 0,
            crc32: crc,
            usize_,
            csize: usize_,
            payload: Payload::Owned(data),
        });
        Ok(())
    }

    fn inflate(&self, e: &Entry) -> Result<Vec<u8>, String> {
        let raw = self.payload(e);
        // защита от zip-бомб: deflate не может раздуть данные больше ~1032:1
        if e.usize_ > e.csize.saturating_mul(MAX_DEFLATE_RATIO).saturating_add(64) {
            return Err(format!("Invalid .akrile file: implausible size for {}", e.name));
        }
        let expected = to_usize(e.usize_)?;
        // лимит +1, чтобы лишний байт не терялся молча и ловился проверкой ниже
        let out = decompress_to_vec_with_limit(raw, expected.saturating_add(1))
            .map_err(|_| format!("Corrupt deflate stream in {}", e.name))?;
        if out.len() != expected {
            return Err(format!("Size mismatch in {}", e.name));
        }
        Ok(out)
    }

    /// Байты файла. Для STORE — без копирования (ссылка на буфер архива).
    fn get_bytes(&self, name: &str) -> Result<Option<Cow<'_, [u8]>>, String> {
        let Some(e) = self.find(name) else {
            return Ok(None);
        };
        let bytes: Cow<'_, [u8]> = if e.method == 0 {
            Cow::Borrowed(self.payload(e))
        } else {
            Cow::Owned(self.inflate(e)?)
        };
        if self.verify_crc && crc32(&bytes) != e.crc32 {
            return Err(format!("CRC mismatch in {}", e.name));
        }
        Ok(Some(bytes))
    }

    fn generate_bytes(&self) -> Vec<u8> {
        // точный размер результата, чтобы не было реаллокаций
        let capacity = HEADER_LEN
            + FOOTER_LEN
            + self
                .entries
                .iter()
                .map(|e| {
                    LOCAL_FIXED_LEN
                        + MIN_CENTRAL_RECORD_LEN
                        + 2 * e.name.len()
                        + e.csize as usize
                })
                .sum::<usize>();

        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());

        let mut central = Vec::with_capacity(self.entries.len() * MIN_CENTRAL_RECORD_LEN);
        for e in &self.entries {
            let offset = out.len() as u64;
            let name = e.name.as_bytes();
            let name_len = (name.len() as u16).to_le_bytes();

            out.extend_from_slice(&name_len);
            out.extend_from_slice(name);
            out.push(e.method);
            out.extend_from_slice(&e.crc32.to_le_bytes());
            out.extend_from_slice(&e.usize_.to_le_bytes());
            out.extend_from_slice(&e.csize.to_le_bytes());
            out.extend_from_slice(self.payload(e));

            central.extend_from_slice(&name_len);
            central.extend_from_slice(name);
            central.push(e.method);
            central.extend_from_slice(&e.crc32.to_le_bytes());
            central.extend_from_slice(&e.usize_.to_le_bytes());
            central.extend_from_slice(&e.csize.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
        }

        let central_offset = out.len() as u64;
        out.extend_from_slice(&central);
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        out.extend_from_slice(END_MAGIC);
        out
    }

    /// Берёт буфер во владение: он становится `source`, записи ссылаются на диапазоны.
    fn parse_bytes(data: Vec<u8>) -> Result<AkrileArchive, String> {
        let entries = Self::parse_entries(&data)?;
        let mut archive = AkrileArchive::new_inner();
        archive.source = data;
        for e in entries {
            archive.insert_entry(e);
        }
        Ok(archive)
    }

    fn parse_entries(data: &[u8]) -> Result<Vec<Entry>, String> {
        if data.len() < MIN_FILE_LEN {
            return Err("Invalid .akrile file: too short".to_string());
        }
        if &data[0..4] != MAGIC {
            return Err("Invalid .akrile file: bad magic".to_string());
        }
        let end = data.len();
        if &data[end - 4..] != END_MAGIC {
            return Err("Invalid .akrile file: bad footer magic".to_string());
        }
        let version = read_u16(data, 4)?;
        if version != VERSION {
            return Err(format!("Unsupported .akrile version: {version}"));
        }

        let central_count = read_u32(data, end - 8)? as usize;
        let central_offset = to_usize(read_u64(data, end - 16)?)?;
        if central_offset < HEADER_LEN || central_offset > end - FOOTER_LEN {
            return Err("Invalid .akrile file: bad central directory offset".to_string());
        }

        // данные записей — до central directory, сам central — до footer
        let body = &data[..central_offset];
        let central = &data[central_offset..end - FOOTER_LEN];

        // Не доверяем счётчику из файла: больше записей, чем влезает в central, быть не может.
        // Иначе with_capacity(u32::MAX) убьёт модуль (panic = "abort").
        if central_count > central.len() / MIN_CENTRAL_RECORD_LEN {
            return Err("Invalid .akrile file: entry count exceeds directory size".to_string());
        }

        let mut entries = Vec::with_capacity(central_count);
        let mut pos = 0usize;
        for _ in 0..central_count {
            let name_len = usize::from(read_u16(central, pos)?);
            pos += 2;
            let name = std::str::from_utf8(slice_checked(central, pos, name_len)?)
                .map_err(|_| "Invalid .akrile file: name is not valid UTF-8".to_string())?
                .to_owned();
            pos += name_len;
            let method = *central
                .get(pos)
                .ok_or_else(|| "Invalid .akrile file: truncated directory".to_string())?;
            pos += 1;
            let crc = read_u32(central, pos)?;
            pos += 4;
            let usize_ = read_u64(central, pos)?;
            pos += 8;
            let csize = read_u64(central, pos)?;
            pos += 8;
            let offset = to_usize(read_u64(central, pos)?)?;
            pos += 8;

            match method {
                0 if usize_ != csize => {
                    return Err("Invalid .akrile file: STORE size mismatch".to_string());
                }
                0 | 1 => {}
                _ => return Err(format!("Unknown compression method: {method}")),
            }

            // local header нужен только чтобы найти начало данных
            let local_name_len = usize::from(read_u16(body, offset)?);
            let start = offset
                .checked_add(LOCAL_FIXED_LEN)
                .and_then(|v| v.checked_add(local_name_len))
                .ok_or_else(|| "Invalid .akrile file: offset overflow".to_string())?;
            let len = to_usize(csize)?;
            slice_checked(body, start, len)?; // проверяем границы один раз здесь

            entries.push(Entry {
                name,
                method,
                crc32: crc,
                usize_,
                csize,
                payload: Payload::Source { start, len },
            });
        }
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// WASM API
// ---------------------------------------------------------------------------

#[wasm_bindgen]
impl AkrileArchive {
    #[wasm_bindgen(constructor)]
    pub fn new() -> AkrileArchive {
        Self::new_inner()
    }

    /// Добавляет файл. `data` передаётся во владение (одна копия JS → WASM).
    /// `level` — 0..=10; `store_only` отключает сжатие.
    #[wasm_bindgen(js_name = addFile)]
    pub fn add_file(
        &mut self,
        name: String,
        data: Vec<u8>,
        store_only: bool,
        level: u8,
    ) -> Result<(), JsValue> {
        self.set_file(name, data, store_only, level)
            .map_err(|e| JsValue::from_str(&e))
    }

    #[wasm_bindgen(js_name = removeFile)]
    pub fn remove_file(&mut self, name: &str) {
        self.remove_entry(name);
    }

    #[wasm_bindgen(js_name = fileNames)]
    pub fn file_names(&self) -> Vec<JsValue> {
        self.entries
            .iter()
            .map(|e| JsValue::from_str(&e.name))
            .collect()
    }

    /// Включает проверку CRC32 при чтении (по умолчанию выключена ради скорости).
    #[wasm_bindgen(js_name = setVerifyCrc)]
    pub fn set_verify_crc(&mut self, verify: bool) {
        self.verify_crc = verify;
    }

    /// Возвращает содержимое файла или `undefined`, если файла нет.
    /// Ошибки (битый поток, CRC, подозрительный размер) бросаются исключением.
    #[wasm_bindgen(js_name = getFile)]
    pub fn get_file(&self, name: &str) -> Result<JsValue, JsValue> {
        match self.get_bytes(name).map_err(|e| JsValue::from_str(&e))? {
            Some(bytes) => Ok(js_sys::Uint8Array::from(&bytes[..]).into()),
            None => Ok(JsValue::UNDEFINED),
        }
    }

    #[wasm_bindgen(js_name = generate)]
    pub fn generate(&self) -> Vec<u8> {
        self.generate_bytes()
    }

    /// Загружает архив. `data` передаётся во владение — повторного копирования нет.
    #[wasm_bindgen(js_name = load)]
    pub fn load(data: Vec<u8>) -> Result<AkrileArchive, JsValue> {
        Self::parse_bytes(data).map_err(|e| JsValue::from_str(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(a: &mut AkrileArchive, name: &str, data: &[u8], store: bool) {
        a.set_file(name.to_string(), data.to_vec(), store, DEFAULT_LEVEL)
            .unwrap();
    }

    fn get(a: &AkrileArchive, name: &str) -> Option<Vec<u8>> {
        a.get_bytes(name).unwrap().map(|c| c.into_owned())
    }

    /// Эталонная побитовая реализация — для сверки быстрой.
    fn crc32_bitwise(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn crc32_matches_bitwise_reference() {
        for len in 0..300usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            assert_eq!(crc32(&data), crc32_bitwise(&data), "len {len}");
        }
    }

    #[test]
    fn round_trip() {
        let mut a = AkrileArchive::new_inner();
        put(&mut a, "hello.txt", b"Hello, akrile world!", false);
        put(&mut a, "raw.bin", &[1, 2, 3, 4, 5], true);
        put(&mut a, "empty.txt", b"", false);

        let loaded = AkrileArchive::parse_bytes(a.generate_bytes()).unwrap();
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(get(&loaded, "hello.txt"), Some(b"Hello, akrile world!".to_vec()));
        assert_eq!(get(&loaded, "raw.bin"), Some(vec![1, 2, 3, 4, 5]));
        assert_eq!(get(&loaded, "empty.txt"), Some(Vec::new()));
        assert_eq!(get(&loaded, "missing"), None);
    }

    #[test]
    fn empty_archive_round_trip() {
        let a = AkrileArchive::new_inner();
        let loaded = AkrileArchive::parse_bytes(a.generate_bytes()).unwrap();
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn overwrite_keeps_one_entry() {
        let mut a = AkrileArchive::new_inner();
        put(&mut a, "a.txt", b"first", true);
        put(&mut a, "a.txt", b"second", true);
        assert_eq!(a.entries.len(), 1);
        assert_eq!(get(&a, "a.txt"), Some(b"second".to_vec()));
    }

    #[test]
    fn remove_keeps_index_consistent() {
        let mut a = AkrileArchive::new_inner();
        put(&mut a, "a.txt", b"A", true);
        put(&mut a, "b.txt", b"B", true);
        put(&mut a, "c.txt", b"C", true);
        a.remove_entry("a.txt");
        assert_eq!(a.entries.len(), 2);
        assert_eq!(get(&a, "a.txt"), None);
        assert_eq!(get(&a, "b.txt"), Some(b"B".to_vec()));
        assert_eq!(get(&a, "c.txt"), Some(b"C".to_vec()));
    }

    #[test]
    fn many_files_round_trip() {
        let mut a = AkrileArchive::new_inner();
        for i in 0..500 {
            let name = format!("dir/file{i}.txt");
            put(&mut a, &name, name.repeat(5).as_bytes(), false);
        }
        let loaded = AkrileArchive::parse_bytes(a.generate_bytes()).unwrap();
        assert_eq!(loaded.entries.len(), 500);
        for i in 0..500 {
            let name = format!("dir/file{i}.txt");
            assert_eq!(get(&loaded, &name), Some(name.repeat(5).into_bytes()));
        }
    }

    #[test]
    fn rejects_bad_magic() {
        let err = AkrileArchive::parse_bytes(b"not an akrile file at all!!".to_vec()).unwrap_err();
        assert!(err.contains("bad magic"), "{err}");
    }

    #[test]
    fn rejects_truncated() {
        let mut a = AkrileArchive::new_inner();
        put(&mut a, "x.txt", b"some data some data some data", false);
        let mut bytes = a.generate_bytes();
        bytes.truncate(bytes.len() - 5);
        assert!(AkrileArchive::parse_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_corrupted_central_offset() {
        let mut a = AkrileArchive::new_inner();
        put(&mut a, "x.txt", b"data", true);
        let mut bytes = a.generate_bytes();
        let n = bytes.len();
        bytes[n - 16..n - 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(AkrileArchive::parse_bytes(bytes).is_err());
    }

    #[test]
    fn rejects_hostile_entry_count() {
        let mut a = AkrileArchive::new_inner();
        put(&mut a, "x.txt", b"data", true);
        let mut bytes = a.generate_bytes();
        let n = bytes.len();
        bytes[n - 8..n - 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = AkrileArchive::parse_bytes(bytes).unwrap_err();
        assert!(err.contains("entry count"), "{err}");
    }

    #[test]
    fn compression_actually_shrinks() {
        let mut a = AkrileArchive::new_inner();
        let data = vec![b'x'; 10_000];
        put(&mut a, "big.txt", &data, false);
        assert_eq!(a.entries[0].method, 1);
        assert!(a.entries[0].csize < 200);
        let loaded = AkrileArchive::parse_bytes(a.generate_bytes()).unwrap();
        assert_eq!(get(&loaded, "big.txt"), Some(data));
    }

    #[test]
    fn precompressed_extensions_are_stored() {
        let mut a = AkrileArchive::new_inner();
        let data = [0u8; 10_000];
        put(&mut a, "photo.PNG", &data, false);
        put(&mut a, "notes.txt", &data, false);
        assert_eq!(a.find("photo.PNG").unwrap().method, 0);
        assert_eq!(a.find("notes.txt").unwrap().method, 1);
    }

    #[test]
    fn verify_crc_detects_corruption() {
        let mut a = AkrileArchive::new_inner();
        put(&mut a, "a.bin", &[1, 2, 3, 4, 5], true);
        let mut bytes = a.generate_bytes();
        let n = bytes.len();
        let central_offset = u64::from_le_bytes(bytes[n - 16..n - 8].try_into().unwrap()) as usize;
        bytes[central_offset - 1] ^= 0xFF; // последний байт данных

        let mut loaded = AkrileArchive::parse_bytes(bytes).unwrap();
        assert!(loaded.get_bytes("a.bin").is_ok()); // по умолчанию не проверяем
        loaded.verify_crc = true;
        let err = loaded.get_bytes("a.bin").unwrap_err();
        assert!(err.contains("CRC mismatch"), "{err}");
    }

    #[test]
    fn rejects_zip_bomb_sizes() {
        let mut a = AkrileArchive::new_inner();
        a.insert_entry(Entry {
            name: "bomb".to_string(),
            method: 1,
            crc32: 0,
            usize_: u64::MAX / 2,
            csize: 4,
            payload: Payload::Owned(vec![0; 4]),
        });
        assert!(a.get_bytes("bomb").is_err());
    }

    #[test]
    fn rejects_wrong_declared_size() {
        let mut a = AkrileArchive::new_inner();
        put(&mut a, "x.txt", &[b'x'; 10_000], false);
        a.entries[0].usize_ = 5_000;
        assert!(a.get_bytes("x.txt").is_err());
    }

    #[test]
    fn rejects_too_long_name() {
        let mut a = AkrileArchive::new_inner();
        assert!(a.set_file("a".repeat(70_000), Vec::new(), true, DEFAULT_LEVEL).is_err());
    }
}
