// 生成图标（assets/ 里的 PNG 已提交，只有想换样子时才重跑）：
//   icon.png            1024×1024 应用图标（electron-builder 转成 .icns）
//   trayTemplate.png    16×16 菜单栏模板图（黑 + alpha，系统按明暗模式着色）
//   trayTemplate@2x.png 32×32
// 图形与网页侧栏的标记同一个（features/layout/sidebar.tsx）：三粒叶绿光斑，"树荫下的账本"（DESIGN.md）；
// 应用图标把它放在纸色的 macOS 圆角方块上。零依赖，自己写 PNG。
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

// 网页标记的 28×28 坐标：大、中、小三粒光斑，透明度 1 / 0.55 / 0.3。
const MOTES = [
  { cx: 11, cy: 12, r: 8, alpha: 1 },
  { cx: 20.5, cy: 18.5, r: 4.5, alpha: 0.55 },
  { cx: 20, cy: 7, r: 2.5, alpha: 0.3 },
];

const mix = (a, b, t) => a.map((v, i) => v + (b[i] - v) * t);
/** 把 top 按 alpha 叠到 base 上（都是 [r, g, b, a]，a 取 0..1）。 */
function over(base, top) {
  const a = top[3] + base[3] * (1 - top[3]);
  if (!a) return [0, 0, 0, 0];
  const c = [0, 1, 2].map((i) => (top[i] * top[3] + base[i] * base[3] * (1 - top[3])) / a);
  return [...c, a];
}

// macOS 图标网格：1024 画布里 824 见方的主体，连续曲率的圆角（超椭圆近似）。
const HALF = 412 / 1024;
const squircle = (x, y) => Math.pow(Math.pow(Math.abs(x - 0.5) / HALF, 5) + Math.pow(Math.abs(y - 0.5) / HALF, 5), 1 / 5);

/** 标记坐标 → 画布坐标：三粒光斑的外框居中，宽占画布的 0.48。 */
function placeMotes(scale, centerX = 14, centerY = 13.75) {
  return MOTES.map((m) => ({ ...m, x: 0.5 + (m.cx - centerX) * scale, y: 0.5 + (m.cy - centerY) * scale, rr: m.r * scale }));
}

const appMotes = placeMotes(0.48 / 22);
function appIcon(x, y) {
  let px = [0, 0, 0, 0];
  // 投影：主体下移一点、边缘柔化
  const sd = (squircle(x, y - 0.01) - 1) * HALF;
  if (sd < 0.024) px = [20, 26, 14, 0.16 * Math.pow(Math.min(1, 1 - sd / 0.024), 2)];
  const f = squircle(x, y);
  if (f > 1) return px;
  // 纸：上亮下略暗；贴边一圈细线，白底上也看得出轮廓
  let paper = [...mix([0xfb, 0xfb, 0xf8], [0xea, 0xec, 0xe4], y), 1];
  if (f > 0.993) paper = [...mix(paper.slice(0, 3), [0xd3, 0xd7, 0xcc], 0.5), 1];
  px = over(px, paper);
  // 大的最后画：叠住中号那一角，不出现半透明叠出来的浑色
  for (const m of [...appMotes].reverse()) {
    const d = Math.hypot(x - m.x, y - m.y);
    if (d > m.rr) continue;
    // 光斑带一点体积：左上略亮、右下略深
    const t = Math.min(1, Math.max(0, ((x - m.x) + (y - m.y)) / (2 * m.rr) + 0.5));
    const color = mix([0x5a, 0x78, 0x41], [0x48, 0x62, 0x33], t);
    px = over(px, [...color, m.alpha]);
  }
  return px;
}
writeFileSync(join(out, "icon.png"), png(1024, appIcon));

// 菜单栏模板图只看 alpha：同样三粒，透明度照搬，系统按明暗着色。外框撑满、留 1px 边。
const trayMotes = placeMotes(0.9 / 22);
const tray = (x, y) => {
  let a = 0;
  for (const m of trayMotes) if (Math.hypot(x - m.x, y - m.y) <= m.rr) a = Math.max(a, m.alpha);
  return [0, 0, 0, a];
};
writeFileSync(join(out, "trayTemplate.png"), png(16, tray));
writeFileSync(join(out, "trayTemplate@2x.png"), png(32, tray));
