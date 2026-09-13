#!/usr/bin/env node
/**
 * zip-to-akrile.js — берёт обычный .zip и переупаковывает в .akrile.
 *
 * Использование:
 *   node zip-to-akrile.js input.zip [output.akrile]
 *
 * Zip читается вручную (central directory), распаковка deflate/store —
 * встроенным zlib. Внешние npm-пакеты не нужны.
 */

import fs from "node:fs";
import zlib from "node:zlib";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Akrile } from "../www/akrile.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const EOCD_SIG = 0x06054b50;
const CEN_SIG = 0x02014b50;

function readZip(buffer) {
  let eocdOffset = -1;
  for (let i = buffer.length - 22; i >= 0; i--) {
    if (buffer.readUInt32LE(i) === EOCD_SIG) {
      eocdOffset = i;
      break;
    }
  }
  if (eocdOffset === -1) {
    throw new Error("Not a valid zip file (End Of Central Directory not found)");
  }

  const totalEntries = buffer.readUInt16LE(eocdOffset + 10);
  const centralDirOffset = buffer.readUInt32LE(eocdOffset + 16);

  const central = [];
  let offset = centralDirOffset;
  for (let i = 0; i < totalEntries; i++) {
    if (buffer.readUInt32LE(offset) !== CEN_SIG) {
      throw new Error(`Bad central directory record at offset ${offset}`);
    }
    const method = buffer.readUInt16LE(offset + 10);
    const compSize = buffer.readUInt32LE(offset + 20);
    const nameLen = buffer.readUInt16LE(offset + 28);
    const extraLen = buffer.readUInt16LE(offset + 30);
    const commentLen = buffer.readUInt16LE(offset + 32);
    const localHeaderOffset = buffer.readUInt32LE(offset + 42);
    const name = buffer.toString("utf8", offset + 46, offset + 46 + nameLen);

    central.push({ name, method, compSize, localHeaderOffset });
    offset += 46 + nameLen + extraLen + commentLen;
  }

  return central.map((e) => {
    const lp = e.localHeaderOffset;
    const lNameLen = buffer.readUInt16LE(lp + 26);
    const lExtraLen = buffer.readUInt16LE(lp + 28);
    const dataStart = lp + 30 + lNameLen + lExtraLen;
    const raw = buffer.subarray(dataStart, dataStart + e.compSize);

    let data;
    if (e.method === 0) data = raw; // stored
    else if (e.method === 8) data = zlib.inflateRawSync(raw); // deflate
    else throw new Error(`Unsupported zip compression method ${e.method} for ${e.name}`);

    return { name: e.name, data };
  });
}

async function convert(inputPath, outputPath) {
  const buffer = fs.readFileSync(inputPath);
  const entries = readZip(buffer);

  const archive = new Akrile();
  for (const entry of entries) {
    if (entry.name.endsWith("/")) {
      archive.folder(entry.name);
    } else {
      archive.file(entry.name, entry.data);
    }
  }

  const akrileBytes = await archive.generateAsync({ type: "uint8array" });
  fs.writeFileSync(outputPath, akrileBytes);

  const origSize = buffer.length;
  const newSize = akrileBytes.length;
  console.log(`OK: ${inputPath} -> ${outputPath}`);
  console.log(`Files: ${entries.length}, ${origSize} -> ${newSize} bytes`);
}

const [, , input, output] = process.argv;
if (!input) {
  console.log("Usage: node zip-to-akrile.js input.zip [output.akrile]");
  process.exit(1);
}

convert(input, output || input.replace(/\.zip$/i, ".akrile")).catch((err) => {
  console.error("Error:", err.message);
  process.exit(1);
});
