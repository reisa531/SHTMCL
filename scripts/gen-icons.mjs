#!/usr/bin/env node
// Dependency-free application icon generator (owned by 00).
//
// `tauri-build` requires a Windows resource icon at
// `apps/desktop/src-tauri/icons/icon.ico`; the bundle also needs PNG sizes.
// This script renders the SHTMCL mark with plain math and encodes PNG/ICO with
// only Node's built-in zlib, so neither build nor CI needs an image toolchain.
//
// Usage: node scripts/gen-icons.mjs [--check]
// `--check` verifies the files exist and decodes their headers without writing.

import { deflateSync } from "node:zlib";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const iconDir = join(scriptDir, "..", "apps", "desktop", "src-tauri", "icons");

// Brand tokens mirrored from apps/desktop/src/styles.css.
const INK = [16, 24, 32, 255];
const SKY = [234, 243, 249, 255];
const SKY_SOFT = [206, 226, 240, 255];
const FOCUS = [47, 111, 190, 255];

const SUPERSAMPLE = 4;

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let index = 0; index < 256; index += 1) {
    let value = index;
    for (let bit = 0; bit < 8; bit += 1) {
      value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
    }
    table[index] = value >>> 0;
  }
  return table;
})();

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([length, body, crc]);
}

function encodePng(size, pixels) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8; // bit depth
  header[9] = 6; // truecolour with alpha
  header[10] = 0; // deflate
  header[11] = 0; // adaptive filtering
  header[12] = 0; // no interlace

  const stride = size * 4;
  const raw = Buffer.alloc((stride + 1) * size);
  for (let y = 0; y < size; y += 1) {
    raw[y * (stride + 1)] = 0; // filter type none
    pixels.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

function encodeIco(entries) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2); // icon
  header.writeUInt16LE(entries.length, 4);

  const directory = Buffer.alloc(entries.length * 16);
  let offset = header.length + directory.length;
  entries.forEach((entry, index) => {
    const base = index * 16;
    directory[base] = entry.size >= 256 ? 0 : entry.size;
    directory[base + 1] = entry.size >= 256 ? 0 : entry.size;
    directory[base + 2] = 0; // palette size
    directory[base + 3] = 0; // reserved
    directory.writeUInt16LE(1, base + 4); // colour planes
    directory.writeUInt16LE(32, base + 6); // bits per pixel
    // Vista+ ICO entries store a PNG payload directly, so no BITMAPINFOHEADER.
    directory.writeUInt32LE(entry.png.length, base + 8);
    directory.writeUInt32LE(offset, base + 12);
    offset += entry.png.length;
  });

  return Buffer.concat([header, directory, ...entries.map((entry) => entry.png)]);
}

// Modern ICNS accepts a PNG-backed 512px image in an ic09 element.
function encodeIcns(png) {
  const header = Buffer.alloc(16);
  header.write("icns", 0, "ascii");
  header.writeUInt32BE(png.length + 16, 4);
  header.write("ic09", 8, "ascii");
  header.writeUInt32BE(png.length + 8, 12);
  return Buffer.concat([header, png]);
}

function blend(target, offset, colour, coverage) {
  if (coverage <= 0) {
    return;
  }
  const alpha = coverage > 1 ? 1 : coverage;
  const inverse = 1 - alpha;
  target[offset] = target[offset] * inverse + colour[0] * alpha;
  target[offset + 1] = target[offset + 1] * inverse + colour[1] * alpha;
  target[offset + 2] = target[offset + 2] * inverse + colour[2] * alpha;
  target[offset + 3] = target[offset + 3] * inverse + colour[3] * alpha;
}

function insideRoundedSquare(x, y, size, radius) {
  const half = size / 2;
  const dx = Math.abs(x - half) - (half - radius);
  const dy = Math.abs(y - half) - (half - radius);
  if (dx <= 0 || dy <= 0) {
    return Math.max(dx, dy) <= radius;
  }
  return Math.hypot(dx, dy) <= radius;
}

function insidePolygon(x, y, points) {
  let inside = false;
  for (let index = 0, previous = points.length - 1; index < points.length; previous = index, index += 1) {
    const [xi, yi] = points[index];
    const [xj, yj] = points[previous];
    const intersects = yi > y !== yj > y && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi;
    if (intersects) {
      inside = !inside;
    }
  }
  return inside;
}

// Isometric cube centred in the tile, in unit coordinates of the tile size.
function cubeFaces() {
  const top = [0.5, 0.235];
  const left = [0.203, 0.415];
  const right = [0.797, 0.415];
  const middle = [0.5, 0.596];
  const bottomLeft = [0.203, 0.592];
  const bottomRight = [0.797, 0.592];
  const bottom = [0.5, 0.773];

  return {
    topFace: [top, right, middle, left],
    leftFace: [left, middle, bottom, bottomLeft],
    rightFace: [middle, right, bottomRight, bottom],
  };
}

function render(size) {
  const scale = SUPERSAMPLE;
  const hi = size * scale;
  const buffer = new Float32Array(hi * hi * 4);

  const radius = hi * 0.223;
  const faces = cubeFaces();
  const toPoints = (points) => points.map(([x, y]) => [x * hi, y * hi]);
  const topFace = toPoints(faces.topFace);
  const leftFace = toPoints(faces.leftFace);
  const rightFace = toPoints(faces.rightFace);

  for (let y = 0; y < hi; y += 1) {
    for (let x = 0; x < hi; x += 1) {
      const px = x + 0.5;
      const py = y + 0.5;
      const offset = (y * hi + x) * 4;
      if (!insideRoundedSquare(px, py, hi, radius)) {
        continue;
      }
      blend(buffer, offset, INK, 1);
      if (insidePolygon(px, py, topFace)) {
        blend(buffer, offset, SKY, 1);
      } else if (insidePolygon(px, py, leftFace)) {
        blend(buffer, offset, SKY_SOFT, 1);
      } else if (insidePolygon(px, py, rightFace)) {
        blend(buffer, offset, FOCUS, 1);
      }
    }
  }

  // Box-downsample the supersampled buffer into the target size.
  const out = Buffer.alloc(size * size * 4);
  const samples = scale * scale;
  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      let r = 0;
      let g = 0;
      let b = 0;
      let a = 0;
      for (let sy = 0; sy < scale; sy += 1) {
        for (let sx = 0; sx < scale; sx += 1) {
          const offset = ((y * scale + sy) * hi + (x * scale + sx)) * 4;
          r += buffer[offset];
          g += buffer[offset + 1];
          b += buffer[offset + 2];
          a += buffer[offset + 3];
        }
      }
      const target = (y * size + x) * 4;
      out[target] = Math.round(r / samples);
      out[target + 1] = Math.round(g / samples);
      out[target + 2] = Math.round(b / samples);
      out[target + 3] = Math.round(a / samples);
    }
  }

  return out;
}

const PNG_TARGETS = [
  ["32x32.png", 32],
  ["128x128.png", 128],
  ["128x128@2x.png", 256],
  ["icon.png", 512],
];

const ICO_TARGETS = [16, 32, 48, 64, 128, 256];

const mode = process.argv[2] ?? "";
if (mode === "--check") {
  let failed = false;
  for (const [name] of PNG_TARGETS) {
    try {
      const bytes = readFileSync(join(iconDir, name));
      const signature = bytes.subarray(0, 8).toString("hex");
      if (signature !== "89504e470d0a1a0a") {
        throw new Error("not a PNG");
      }
      console.log(`ok ${name} ${bytes.length} bytes`);
    } catch (error) {
      failed = true;
      console.error(`missing ${name}: ${error.message}`);
    }
  }
  try {
    const ico = readFileSync(join(iconDir, "icon.ico"));
    const count = ico.readUInt16LE(4);
    console.log(`ok icon.ico ${ico.length} bytes, ${count} entries`);
  } catch (error) {
    failed = true;
    console.error(`missing icon.ico: ${error.message}`);
  }
  try {
    const icns = readFileSync(join(iconDir, "icon.icns"));
    if (icns.subarray(0, 4).toString() !== "icns" || icns.readUInt32BE(4) !== icns.length
      || icns.subarray(8, 12).toString() !== "ic09" || icns.readUInt32BE(12) !== icns.length - 8
      || icns.subarray(16, 24).toString("hex") !== "89504e470d0a1a0a"
      || icns.readUInt32BE(32) !== 512 || icns.readUInt32BE(36) !== 512) {
      throw new Error("invalid 512px ICNS");
    }
    console.log(`ok icon.icns ${icns.length} bytes`);
  } catch (error) {
    failed = true;
    console.error(`missing icon.icns: ${error.message}`);
  }
  process.exitCode = failed ? 1 : 0;
} else {
  mkdirSync(iconDir, { recursive: true });
  for (const [name, size] of PNG_TARGETS) {
    const png = encodePng(size, render(size));
    writeFileSync(join(iconDir, name), png);
    console.log(`wrote ${name} (${size}px, ${png.length} bytes)`);
  }
  const entries = ICO_TARGETS.map((size) => ({ size, png: encodePng(size, render(size)) }));
  const ico = encodeIco(entries);
  writeFileSync(join(iconDir, "icon.ico"), ico);
  console.log(`wrote icon.ico (${ICO_TARGETS.join(", ")}px, ${ico.length} bytes)`);
  const icns = encodeIcns(readFileSync(join(iconDir, "icon.png")));
  writeFileSync(join(iconDir, "icon.icns"), icns);
  console.log(`wrote icon.icns (512px, ${icns.length} bytes)`);
}
