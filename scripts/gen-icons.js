// 从 DSH 前端 favicon.svg 提取 DeepSeek 鲸鱼 logo（原始 viewBox 0 0 50 50，
// 鲸鱼实际范围 x≈0.5..49.3, y≈0..43.3）
// 生成两套 PNG：
//   1) 根目录 app-icon.png —— 应用图标源 1024x1024（蓝色渐变圆角底 + 白色鲸鱼居中，留 ~15% 边距）
//   2) src-tauri/icons/tray.png —— 托盘图标 64x64（透明底 + 品牌蓝鲸鱼居中）
const fs = require("fs");
const path = require("path");
const { Resvg } = require("@resvg/resvg-js");

const { execFileSync } = require("child_process");

const root = path.resolve(__dirname, "..");

// Locate the dsh frontend's favicon.svg. This used to be a hardcoded absolute
// path into one machine's global node_modules, which broke this script for
// everyone else (and for that machine as soon as Node was upgraded).
const FAVICON_PKG = "@deepseek-ai/dsh-web-frontend/dist/favicon.svg";

function npmGlobalRoot() {
  try {
    return execFileSync("npm", ["root", "-g"], {
      encoding: "utf8",
      shell: process.platform === "win32",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  } catch {
    return null;
  }
}

function resolveFavicon() {
  // 1) Explicit override always wins.
  if (process.env.DSH_FAVICON) return process.env.DSH_FAVICON;

  const tried = [];

  // 2) Normal resolution: works when dsh is a local dependency.
  try {
    return require.resolve(FAVICON_PKG);
  } catch {
    tried.push(`require.resolve("${FAVICON_PKG}")`);
  }

  // 3) Global install: dsh nests the frontend under its own node_modules.
  const globalRoot = npmGlobalRoot();
  if (globalRoot) {
    const candidates = [
      path.join(globalRoot, "@deepseek-ai/dsh/node_modules", FAVICON_PKG),
      path.join(globalRoot, FAVICON_PKG),
    ];
    for (const candidate of candidates) {
      if (fs.existsSync(candidate)) return candidate;
      tried.push(candidate);
    }
  } else {
    tried.push("npm root -g (command failed)");
  }

  throw new Error(
    `找不到 dsh 前端的 favicon.svg。已尝试：\n  ${tried.join("\n  ")}\n` +
      `请先安装 dsh（npm i -g @deepseek-ai/dsh），` +
      `或用 DSH_FAVICON=<path-to-favicon.svg> 指定路径。`
  );
}

const faviconPath = resolveFavicon();
console.log("favicon:", faviconPath);
const favicon = fs.readFileSync(faviconPath, "utf8");

const m = favicon.match(/\sd="([^"]+)"/);
if (!m) throw new Error("path not found in favicon.svg");
const d = m[1];
console.log("path length:", d.length);

const BRAND_BLUE = "#4D6BFE";
const BRAND_DARK = "#3652C9";

// 鲸鱼在 50x50 坐标系中的真实范围
const MIN_X = 0.5, MAX_X = 49.3, MIN_Y = 0.0, MAX_Y = 43.3;

// 居中变换：scale 取「宽度因子/高度因子」中较小的那个，确保两维都不出界
function centerTransform(canvasW, canvasH, widthFactor, heightFactor, dy = 0) {
  const w = MAX_X - MIN_X;
  const h = MAX_Y - MIN_Y;
  const scale = Math.min((canvasW * widthFactor) / w, (canvasH * heightFactor) / h);
  const tx = (canvasW - w * scale) / 2 - MIN_X * scale;
  const ty = (canvasH - h * scale) / 2 - MIN_Y * scale + dy;
  return `translate(${tx.toFixed(2)} ${ty.toFixed(2)}) scale(${scale})`;
}

// ---- 1) 应用图标 1024x1024 ----
const APP = 1024;
// 鲸鱼占画布宽 68%、高 74%，dy 上移 45px 平衡光学重心
const appSvg = `<svg xmlns="http://www.w3.org/2000/svg" width="${APP}" height="${APP}" viewBox="0 0 ${APP} ${APP}">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="${BRAND_BLUE}"/>
      <stop offset="1" stop-color="${BRAND_DARK}"/>
    </linearGradient>
    <radialGradient id="glow" cx="0.5" cy="0.38" r="0.9">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.16"/>
      <stop offset="1" stop-color="#ffffff" stop-opacity="0"/>
    </radialGradient>
  </defs>
  <rect x="0" y="0" width="${APP}" height="${APP}" rx="220" ry="220" fill="url(#bg)"/>
  <rect x="0" y="0" width="${APP}" height="${APP}" rx="220" ry="220" fill="url(#glow)"/>
  <g transform="${centerTransform(APP, APP, 0.68, 0.74, -45)}">
    <path d="${d}" fill="#ffffff"/>
  </g>
</svg>`;

// ---- 2) 托盘图标 64x64（透明底，品牌蓝鲸鱼，Windows 托盘浅色背景可见）----
const TRAY = 64;
const traySvg = `<svg xmlns="http://www.w3.org/2000/svg" width="${TRAY}" height="${TRAY}" viewBox="0 0 ${TRAY} ${TRAY}">
  <g transform="${centerTransform(TRAY, TRAY, 0.78, 0.78)}">
    <path d="${d}" fill="${BRAND_BLUE}"/>
  </g>
</svg>`;

function render(svg, outPath, w) {
  const resvg = new Resvg(svg, { fitTo: { mode: "width", value: w } });
  const png = resvg.render().asPng();
  fs.writeFileSync(outPath, png);
  console.log("wrote", outPath, png.length, "bytes");
}

render(appSvg, path.join(root, "app-icon.png"), APP);
render(traySvg, path.join(root, "src-tauri", "icons", "tray.png"), TRAY);