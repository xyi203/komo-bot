// 一次性生成占位图标（assets/ 里的 PNG 已提交，只有想换样子时才重跑）：
//   icon.png            1024×1024 应用图标（electron-builder 转成 .icns）
//   trayTemplate.png    16×16 菜单栏模板图（黑 + alpha，系统按明暗模式着色）
//   trayTemplate@2x.png 32×32
// 没有设计稿之前用最简单的形状：圆角方块里一个圆点。零依赖，自己写 PNG。
import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const out = join(dirname(fileURLToPath(import.meta.url)), "..", "assets");
mkdirSync(out, { recursive: true });

const crcTable = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
const crc32 = (buf) => {
  let c = 0xffffffff;
  for (const byte of buf) c = crcTable[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
};
const chunk = (type, data) => {
  const body = Buffer.concat([Buffer.from(type), data]);
  const head = Buffer.alloc(4);
  head.writeUInt32BE(data.length);
  const tail = Buffer.alloc(4);
  tail.writeUInt32BE(crc32(body));
  return Buffer.concat([head, body, tail]);
};
function png(size, pixel) {
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    for (let x = 0; x < size; x++) {
      // 4×4 超采样抗锯齿
      let r = 0, g = 0, b = 0, a = 0;
      for (let sy = 0; sy < 4; sy++)
        for (let sx = 0; sx < 4; sx++) {
          const p = pixel((x + (sx + 0.5) / 4) / size, (y + (sy + 0.5) / 4) / size);
          r += p[0] * p[3]; g += p[1] * p[3]; b += p[2] * p[3]; a += p[3];
        }
      const o = y * (size * 4 + 1) + 1 + x * 4;
      raw[o] = a ? Math.round(r / a) : 0;
      raw[o + 1] = a ? Math.round(g / a) : 0;
      raw[o + 2] = a ? Math.round(b / a) : 0;
      raw[o + 3] = Math.round((a / 16) * 255);
    }
  }
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8;
  header[9] = 6;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
};

const inRoundRect = (x, y, inset, radius) => {
  const lo = inset, hi = 1 - inset;
  if (x < lo || x > hi || y < lo || y > hi) return false;
  const cx = Math.min(Math.max(x, lo + radius), hi - radius);
  const cy = Math.min(Math.max(y, lo + radius), hi - radius);
  return Math.hypot(x - cx, y - cy) <= radius;
};
const inCircle = (x, y, r) => Math.hypot(x - 0.5, y - 0.5) <= r;

writeFileSync(
  join(out, "icon.png"),
  png(1024, (x, y) =>
    inRoundRect(x, y, 0.08, 0.2) ? (inCircle(x, y, 0.2) ? [250, 250, 250, 1] : [28, 28, 30, 1]) : [0, 0, 0, 0],
  ),
);
const tray = (x, y) => (inRoundRect(x, y, 0.06, 0.28) && !inCircle(x, y, 0.2) ? [0, 0, 0, 1] : [0, 0, 0, 0]);
writeFileSync(join(out, "trayTemplate.png"), png(16, tray));
writeFileSync(join(out, "trayTemplate@2x.png"), png(32, tray));
