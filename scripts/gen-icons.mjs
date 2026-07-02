#!/usr/bin/env node
// Quarterdeck icon generator (T0). Pure Node, no native deps.
//
// Produces:
//   assets/tray/{green,yellow,red,gray}-{16,32}.png  -- status tray icons
//   assets/tray/{green,yellow,red,gray}.ico          -- Windows tray variants
//   assets/app/icon-512.png                          -- neutral app source
//   src-tauri/icons/32x32.png, 128x128.png,
//                   128x128@2x.png, icon.ico          -- Tauri bundle icons
//
// PNGs are RGBA (color type 6) deflated with Node's zlib. ICOs embed PNG frames
// (valid on Windows Vista+). Anti-aliased filled circles via 4x4 supersampling.

import zlib from 'node:zlib';
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, '..');

// ---- PNG encoding -------------------------------------------------------

const crcTable = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = crcTable[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, 'ascii');
  const crcBuf = Buffer.alloc(4);
  crcBuf.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
  return Buffer.concat([len, typeBuf, data, crcBuf]);
}

function encodePNG(width, height, rgba) {
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type RGBA
  ihdr[10] = 0; // compression
  ihdr[11] = 0; // filter
  ihdr[12] = 0; // interlace
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  for (let y = 0; y < height; y++) {
    raw[y * (stride + 1)] = 0; // filter: none
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, y * stride + stride);
  }
  const idat = zlib.deflateSync(raw, { level: 9 });
  return Buffer.concat([
    sig,
    chunk('IHDR', ihdr),
    chunk('IDAT', idat),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

// ---- drawing ------------------------------------------------------------

// Anti-aliased filled disc. `fill` = [r,g,b]; optional lighter `rim`.
function drawDisc(size, fill, rim) {
  const rgba = Buffer.alloc(size * size * 4);
  const cx = size / 2;
  const cy = size / 2;
  const r = size / 2 - Math.max(1, size * 0.08);
  const rimR = r - Math.max(1, size * 0.14);
  const SS = 4;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let inside = 0;
      let inner = 0;
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const px = x + (sx + 0.5) / SS;
          const py = y + (sy + 0.5) / SS;
          const d = Math.hypot(px - cx, py - cy);
          if (d <= r) inside++;
          if (rim && d <= rimR) inner++;
        }
      }
      const total = SS * SS;
      const a = inside / total;
      const i = (y * size + x) * 4;
      let col = fill;
      if (rim && inner > 0) {
        const t = inner / total;
        col = [
          Math.round(fill[0] + (rim[0] - fill[0]) * t),
          Math.round(fill[1] + (rim[1] - fill[1]) * t),
          Math.round(fill[2] + (rim[2] - fill[2]) * t),
        ];
      }
      rgba[i] = col[0];
      rgba[i + 1] = col[1];
      rgba[i + 2] = col[2];
      rgba[i + 3] = Math.round(a * 255);
    }
  }
  return rgba;
}

function discPNG(size, fill, rim) {
  return encodePNG(size, size, drawDisc(size, fill, rim));
}

// ---- ICO encoding -------------------------------------------------------

function encodeICO(frames) {
  // frames: [{ size, png }]
  const count = frames.length;
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0); // reserved
  header.writeUInt16LE(1, 2); // type: icon
  header.writeUInt16LE(count, 4);
  const entries = [];
  const datas = [];
  let offset = 6 + count * 16;
  for (const f of frames) {
    const e = Buffer.alloc(16);
    e[0] = f.size >= 256 ? 0 : f.size; // width (0 => 256)
    e[1] = f.size >= 256 ? 0 : f.size; // height
    e[2] = 0; // palette
    e[3] = 0; // reserved
    e.writeUInt16LE(1, 4); // color planes
    e.writeUInt16LE(32, 6); // bits per pixel
    e.writeUInt32LE(f.png.length, 8);
    e.writeUInt32LE(offset, 12);
    offset += f.png.length;
    entries.push(e);
    datas.push(f.png);
  }
  return Buffer.concat([header, ...entries, ...datas]);
}

// ---- palette ------------------------------------------------------------

// Status colors (dark-theme values from SPEC §7) plus a light rim for depth.
const STATUS = {
  green: [63, 185, 80],
  yellow: [210, 153, 34],
  red: [248, 81, 73],
  gray: [110, 118, 129],
};
const CLAY = [217, 119, 87]; // brand accent for the neutral app icon
const CLAY_RIM = [240, 165, 140];

// ---- write --------------------------------------------------------------

function out(rel, buf) {
  const p = join(root, rel);
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(p, buf);
  console.log(`  ${rel}  (${buf.length} bytes)`);
}

console.log('Generating Quarterdeck icons...');

// Status tray icons: 16 + 32 PNG, plus a multi-size ICO each.
for (const [name, color] of Object.entries(STATUS)) {
  const rim = [
    Math.min(255, color[0] + 60),
    Math.min(255, color[1] + 60),
    Math.min(255, color[2] + 60),
  ];
  const png16 = discPNG(16, color, rim);
  const png32 = discPNG(32, color, rim);
  out(`assets/tray/${name}-16.png`, png16);
  out(`assets/tray/${name}-32.png`, png32);
  out(
    `assets/tray/${name}.ico`,
    encodeICO([
      { size: 16, png: png16 },
      { size: 32, png: png32 },
    ]),
  );
}

// Neutral app icon (clay disc with a soft rim).
const app512 = discPNG(512, CLAY, CLAY_RIM);
out('assets/app/icon-512.png', app512);

// Tauri bundle icon set.
out('src-tauri/icons/32x32.png', discPNG(32, CLAY, CLAY_RIM));
out('src-tauri/icons/128x128.png', discPNG(128, CLAY, CLAY_RIM));
out('src-tauri/icons/128x128@2x.png', discPNG(256, CLAY, CLAY_RIM));
out('src-tauri/icons/icon.png', app512);
out(
  'src-tauri/icons/icon.ico',
  encodeICO([
    { size: 16, png: discPNG(16, CLAY, CLAY_RIM) },
    { size: 32, png: discPNG(32, CLAY, CLAY_RIM) },
    { size: 48, png: discPNG(48, CLAY, CLAY_RIM) },
    { size: 256, png: discPNG(256, CLAY, CLAY_RIM) },
  ]),
);

console.log('Done.');
