# akrile

Свой архивный формат `.akrile` (сжатие deflate, как в zip) + WASM-библиотека
на Rust с JS-обёрткой, API которой намеренно повторяет **JSZip** — чтобы
заменить в проекте `JSZip` на `Akrile` практически без правок.

## Структура проекта

```
akrile/
├── Cargo.toml           # Rust-крейт (компилируется в WASM)
├── src/lib.rs            # реализация формата + wasm-bindgen API
├── www/
│   ├── akrile.js          # JS-обёртка с JSZip-подобным API
│   └── pkg/               # сюда попадёт сборка wasm-pack (генерируется)
├── examples/
│   └── zip-to-akrile.js  # Node-скрипт: конвертирует .zip → .akrile
└── package.json
```

## Сборка (нужен интернет для скачивания крейтов)

Требуется Rust + `wasm-pack`:

```bash
cargo install wasm-pack   # один раз, если ещё не установлен

wasm-pack build --target web --out-dir www/pkg
```

После этого в `www/pkg/` появятся `akrile_bg.wasm`, `akrile.js` (glue-код
wasm-bindgen) и `.d.ts` — их подключает `www/akrile.js`. Одна и та же сборка
(`--target web`) используется и в браузере, и в Node — `www/akrile.js` сам
определяет окружение и в Node читает `.wasm` через `fs` вместо `fetch()`.

> В этой песочнице нет доступа к сети, поэтому собрать `.wasm` и прогнать
> `wasm-pack` прямо здесь я не могу — но исходники рабочие и собираются
> одной командой выше. Чистая логика формата (без wasm-bindgen) покрыта
> юнит-тестами и собирается/тестируется нативно, без wasm-pack:
>
> ```bash
> cargo test --lib
> ```

Есть готовый GitHub Actions workflow (`.github/workflows/build.yml`),
который на каждый push/PR гоняет `cargo test`, `cargo clippy`, собирает
wasm и кладёт готовую библиотеку в артефакты сборки.

## Использование (API как у JSZip)

```js
import { Akrile } from "./www/akrile.js";

await Akrile.ready; // дождаться инициализации wasm (один раз на странице)

const zip = new Akrile();
zip.file("hello.txt", "Привет, мир!");
zip.folder("images").file("logo.png", pngBytes);

const blob = await zip.generateAsync({ type: "blob" }); // .akrile файл

// --- чтение ---
const loaded = await Akrile.loadAsync(blob);
const text = await loaded.file("hello.txt").async("string");
loaded.forEach((name, file) => console.log(name));
```

Поддерживаемые типы в `generateAsync`/`file().async()`:
`"uint8array"`, `"arraybuffer"`, `"string"`/`"text"`, `"blob"`, `"base64"`.

Отличия от JSZip (сознательно упрощено):
- `folder()` возвращает объект с тем же интерфейсом, но без полноценного
  дерева — все файлы физически хранятся в одном общем архиве с префиксом пути.
- Нет стриминга (`generateInternalStream`) и NodeJS `Readable` — только
  «всё в память», архив на десятки/сотни МБ ворочать так не стоит.
- Опции сжатия: `{ compression: "STORE" }` — без сжатия, по умолчанию —
  deflate (уровень 6, как разумный баланс скорость/размер).

## Конвертация zip → akrile

```bash
node examples/zip-to-akrile.js input.zip output.akrile
```

Скрипт сам разбирает central directory обычного zip, распаковывает store/deflate
через встроенный `zlib`, и запаковывает файлы в `.akrile` через ту же
JS-обёртку. Собран на Node ESM, зависимостей кроме `www/pkg-node` (см. сборку
выше) не требует.

## Формат `.akrile`

```
[HEADER]
  magic         4 bytes  "AKRL"
  version       1 byte   = 1
  flags         1 byte   зарезервировано
  entry_count   4 bytes  LE u32

[ENTRY DATA] × entry_count, подряд
  name_len      2 bytes  LE u16
  name          UTF-8, name_len байт
  method        1 byte   0 = store, 1 = deflate
  crc32         4 bytes  LE u32   (crc исходных данных)
  uncompressed  8 bytes  LE u64
  compressed    8 bytes  LE u64
  data          compressed байт

[CENTRAL DIRECTORY] × entry_count
  name_len, name, method, crc32, uncompressed, compressed  — как выше
  offset        8 bytes  LE u64  (смещение записи в ENTRY DATA)

[FOOTER]
  central_dir_offset  8 bytes LE u64
  central_dir_count   4 bytes LE u32
  end_magic           4 bytes  "AKRE"
```

Central directory в конце файла позволяет читать список файлов и доставать
любой конкретный файл без разбора всего архива — как в zip.

## Возможные улучшения (не реализовано)

- Шифрование содержимого (AES) на уровне записи.
- Потоковая распаковка больших файлов (сейчас всё через `Vec<u8>` в памяти).
- Уровни сжатия, настраиваемые из JS (`options.level`).
- Полноценное объектное дерево папок вместо плоского списка с префиксами.
