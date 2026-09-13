/**
 * akrile.js — обёртка над WASM-ядром с API, максимально похожим на JSZip.
 *
 * Основные отличия от JSZip минимальны специально, чтобы можно было
 * заменить `new JSZip()` на `new Akrile()` почти без правки кода:
 *
 *   const zip = new Akrile();
 *   zip.file("hello.txt", "Hello world!");
 *   zip.folder("images").file("a.png", bytes);   // упрощённая поддержка папок
 *   const blob = await zip.generateAsync({ type: "blob" });
 *
 *   const loaded = await Akrile.loadAsync(blob);
 *   const text = await loaded.file("hello.txt").async("string");
 *
 * ВАЖНО: перед использованием нужно один раз дождаться инициализации WASM:
 *   await Akrile.ready;
 */

import initWasm, { AkrileArchive } from "./pkg/akrile.js";

let readyPromise = null;

// Один и тот же www/pkg (собранный `wasm-pack build --target web`) должен
// работать и в браузере, и в Node — а generated init() по умолчанию делает
// fetch() соседнего .wasm файла, что в Node не работает без явных байт.
// Поэтому в Node читаем .wasm вручную через fs и передаём в initWasm().
async function ensureInit() {
  if (readyPromise) return readyPromise;

  readyPromise = (async () => {
    const isNode = typeof process !== "undefined" && !!process.versions?.node;
    if (isNode) {
      const { readFileSync } = await import("node:fs");
      const wasmUrl = new URL("./pkg/akrile_bg.wasm", import.meta.url);
      await initWasm(readFileSync(wasmUrl));
    } else {
      await initWasm();
    }
  })();

  return readyPromise;
}

function toBytes(data) {
  if (typeof data === "string") return new TextEncoder().encode(data);
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  throw new Error("akrile: unsupported data type, use string/Uint8Array/ArrayBuffer");
}

function convertOut(u8, type = "uint8array") {
  switch (type) {
    case "uint8array":
      return u8;
    case "arraybuffer":
      return u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);
    case "string":
    case "text":
      return new TextDecoder().decode(u8);
    case "blob":
      return new Blob([u8]);
    case "base64": {
      let bin = "";
      for (let i = 0; i < u8.length; i++) bin += String.fromCharCode(u8[i]);
      return btoa(bin);
    }
    default:
      throw new Error(`akrile: unsupported output type "${type}"`);
  }
}

class AkrileFile {
  constructor(name, zip) {
    this.name = name;
    this._zip = zip;
  }

  /** Аналог JSZipObject#async(type) */
  async async(type = "uint8array") {
    await ensureInit();
    const bytes = this._zip._inner.getFile(this.name);
    if (!bytes) throw new Error(`akrile: file not found in archive: ${this.name}`);
    return convertOut(bytes, type);
  }
}

export class Akrile {
  constructor() {
    this._inner = new AkrileArchive();
    this._prefix = "";
    this.files = {};
  }

  /**
   * file(name)            -> AkrileFile | null   (чтение)
   * file(name, data, opts) -> this                (запись, opts.compression = "STORE"|"DEFLATE")
   */
  file(name, data, options = {}) {
    const fullName = this._prefix + name;

    if (data === undefined) {
      return this.files[fullName] || null;
    }

    const bytes = toBytes(data);
    const store = options.compression === "STORE";
    this._inner.addFile(fullName, bytes, store);
    this.files[fullName] = new AkrileFile(fullName, this);
    return this;
  }

  /** Упрощённая поддержка папок: возвращает "поднабор" с тем же интерфейсом. */
  folder(name) {
    const folderName = (this._prefix + name).replace(/\/?$/, "/");
    this._inner.addFile(folderName, new Uint8Array(0), true);
    this.files[folderName] = new AkrileFile(folderName, this);

    const sub = Object.create(Akrile.prototype);
    sub._inner = this._inner;
    sub._prefix = folderName;
    sub.files = this.files;
    return sub;
  }

  remove(name) {
    const fullName = this._prefix + name;
    this._inner.removeFile(fullName);
    delete this.files[fullName];
    return this;
  }

  forEach(callback) {
    for (const name of Object.keys(this.files)) {
      if (this._prefix && !name.startsWith(this._prefix)) continue;
      callback(name, this.files[name]);
    }
  }

  /** Аналог JSZip#generateAsync({ type }) */
  async generateAsync(options = {}) {
    await ensureInit();
    const bytes = this._inner.generate();
    return convertOut(bytes, options.type || "uint8array");
  }

  /** Аналог JSZip.loadAsync(data) */
  static async loadAsync(data) {
    await ensureInit();

    let bytes;
    if (typeof data === "string") {
      bytes = Uint8Array.from(atob(data), (c) => c.charCodeAt(0));
    } else if (data instanceof Blob) {
      bytes = new Uint8Array(await data.arrayBuffer());
    } else {
      bytes = toBytes(data);
    }

    const zip = new Akrile();
    zip._inner = AkrileArchive.load(bytes);
    for (const name of zip._inner.fileNames()) {
      zip.files[name] = new AkrileFile(name, zip);
    }
    return zip;
  }
}

Akrile.ready = ensureInit();

export default Akrile;
