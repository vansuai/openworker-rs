import { chromium } from 'file:///Users/ygqbasic/Documents/source/product/openworker/surfaces/gui/node_modules/playwright/index.mjs';
import { fileURLToPath } from 'url';
import { dirname } from 'path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const previewPath = `file://${__dirname}/../surfaces/gui/assets/brand/_preview.html`;
const outDir = '/Users/ygqbasic/.codex/visualizations/2026/08/07/019fdbff-b81d-7142-b2b2-fb6cccebd716';

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1280, height: 1800 }, deviceScaleFactor: 2 });
const page = await ctx.newPage();
await page.goto(previewPath, { waitUntil: 'networkidle' });

// Full overview
await page.screenshot({ path: `${outDir}/vansu-overview.png`, fullPage: true });

// Individual tiles
const captures = [
  { src: 'logo-mark.svg',            size: 512, file: 'vansu-mark-512.png',          bg: '#FFFFFF', fg: '#1B2440' },
  { src: 'logo-mark.svg',            size: 512, file: 'vansu-mark-on-dark-512.png',   bg: '#0E1A2E', fg: '#F4ECD8' },
  { src: 'logo-mark-color.svg',      size: 512, file: 'vansu-mark-color-512.png',     bg: '#F2EFE8' },
  { src: 'logo-mark-color.svg',      size: 1024, file: 'vansu-app-icon-1024.png',    bg: '#F2EFE8' },
  { src: 'favicon.svg',              size: 64,  file: 'vansu-favicon-64.png',        bg: '#FFFFFF', fg: '#1B2440' },
  { src: 'logo-lockup.svg',          size: 660, file: 'vansu-lockup-h-660.png',      bg: '#FFFFFF', fg: '#1B2440' },
  { src: 'logo-lockup-stacked.svg',  size: 360, file: 'vansu-lockup-stack-360.png',  bg: '#FFFFFF', fg: '#1B2440' },
];

for (const c of captures) {
  const html = `<!doctype html><html><head><style>
    html,body{margin:0;padding:0;background:${c.bg};}
    .wrap{padding:48px;display:flex;align-items:center;justify-content:center;color:${c.fg};}
    img{width:${c.size}px;height:auto;display:block;}
  </style></head><body><div class="wrap"><img src="file://${__dirname}/../surfaces/gui/assets/brand/${c.src}"></div></body></html>`;
  const p = await ctx.newPage();
  await p.setContent(html, { waitUntil: 'networkidle' });
  // Resize viewport to fit
  const vp = await p.evaluate(() => {
    const el = document.querySelector('img');
    const r = el.getBoundingClientRect();
    return { width: Math.ceil(r.width + 96), height: Math.ceil(r.height + 96) };
  });
  await p.setViewportSize(vp);
  await p.screenshot({ path: `${outDir}/${c.file}`, omitBackground: false });
  await p.close();
}

await browser.close();
console.log('Rendered to', outDir);
