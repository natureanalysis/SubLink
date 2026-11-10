#!/usr/bin/env node
'use strict';
/**
 * verify-req3.cjs —— 文件夹级容量上限的界面取证（需求 3，行内编辑版）。
 *
 * 界面已取消规则表单弹窗：容量上限直接在列表单元格里编辑。本脚本用 ?mock=1 起界面，
 * 程序化测量而不是“看图猜”：
 *   1) 单击容量上限单元格后，「数值 + 单位」控件宽度是否可用（不被挤扁）；
 *   2) 编辑既有规则时能正确回填（2 GB → 数值 2 + 单位 GB）；
 *   3) 填了上限却没选策略时，保存会被前端拦下并给出警示。
 *
 * 用法： node scripts/verify-req3.cjs
 * 依赖： playwright（见 take-screens.cjs 的 NODE_PATH 说明）
 */

const path = require('path');
const url = require('url');

const ROOT = path.resolve(__dirname, '..');
const UI_INDEX = path.join(ROOT, 'crates', 'sl-gui', 'ui', 'index.html');

let chromium;
try {
  chromium = require('playwright').chromium;
} catch (e) {
  console.error('[verify-req3] 缺少 playwright：' + e.message);
  process.exit(1);
}

const checks = [];
function ok(name, cond, detail) {
  checks.push({ name: name, pass: !!cond, detail: detail });
  console.log((cond ? '  PASS  ' : '  FAIL  ') + name + (detail ? '  → ' + detail : ''));
}

async function launch() {
  for (const channel of ['msedge', 'chrome', null]) {
    try {
      return await chromium.launch(Object.assign({ headless: true }, channel ? { channel } : {}));
    } catch (e) { /* 试下一个 */ }
  }
  throw new Error('没有可用的浏览器（试过 Edge / Chrome / 自带 chromium）');
}

(async () => {
  const browser = await launch();
  const context = await browser.newContext({ locale: 'zh-CN', viewport: { width: 1600, height: 900 } });
  await context.addInitScript(() => {
    try { localStorage.setItem('sublink.locale', 'zh'); } catch (e) {}
  });
  const page = await context.newPage();
  const uiUrl = url.pathToFileURL(UI_INDEX).href + '?mock=1';

  await page.goto(uiUrl, { waitUntil: 'load' });
  await page.waitForSelector('#rows tr[data-id]');

  // ---- 1) 行内容量上限控件与回填 ----
  await page.click('#rows tr[data-id="1"] td[data-field="clean_limit"]');
  await page.waitForSelector('#rows tr[data-id="1"] td[data-field="clean_limit"] .cell-editor');
  const geom = await page.evaluate(() => {
    const td = document.querySelector('#rows tr[data-id="1"] td[data-field="clean_limit"]');
    const n = td.querySelector('input[type="number"]');
    const s = td.querySelector('select');
    const rn = n.getBoundingClientRect();
    const rs = s.getBoundingClientRect();
    return {
      numW: Math.round(rn.width), selW: Math.round(rs.width),
      numScroll: n.scrollWidth, numClient: n.clientWidth,
      value: n.value, unit: s.value,
    };
  });
  ok('行内存在「容量上限 + 单位」控件', !!geom, JSON.stringify(geom));
  ok('数值输入框宽度可用（>=40px）', geom.numW >= 40, geom.numW + 'px');
  ok('单位选择框宽度可用（>=40px）', geom.selW >= 40, geom.selW + 'px');
  ok('数值无横向截断', geom.numScroll <= geom.numClient + 1, geom.numScroll + '/' + geom.numClient);
  ok('编辑既有规则回填正确（2 / GB）', geom.value === '2' && geom.unit === 'gb', geom.value + ' / ' + geom.unit);
  await page.keyboard.press('Escape');
  await page.waitForTimeout(100);

  // ---- 2) 填上限但不选策略 → 保存被拦下 + 警示 ----
  await page.click('#btn-add');
  await page.waitForSelector('#rows tr.draft-row');
  await page.fill('#rows tr.draft-row [data-draft-field="name"] input', '上限校验');
  await page.fill('#rows tr.draft-row [data-draft-field="source"] input', '%LOCALAPPDATA%\\LimitCheck');
  await page.fill('#rows tr.draft-row [data-draft-field="target"] input', 'dev\\limit-check');
  await page.fill('#rows tr.draft-row [data-draft-field="clean_limit"] input[type="number"]', '2');
  await page.click('#rows tr.draft-row button.mini.primary');
  await page.waitForSelector('#toasts .toast, #toasts > *', { timeout: 3000 }).catch(() => {});
  await page.waitForTimeout(150);
  const toastText = await page.evaluate(() => {
    const host = document.querySelector('#toasts');
    return host ? host.textContent : '';
  });
  ok('上限 > 0 且无策略时保存被拦下', /选择策略|必须选|requires/i.test(toastText), toastText.slice(0, 80));
  ok('草稿行保留，未丢失已填内容', await page.isVisible('#rows tr.draft-row'));
  ok('全程未出现规则弹窗', await page.isHidden('#modal'));

  await browser.close();

  const failed = checks.filter((c) => !c.pass);
  console.log('\n[verify-req3] ' + (failed.length ? 'FAILED (' + failed.length + ')' : 'VERIFY_PASS'));
  process.exit(failed.length ? 1 : 0);
})().catch((e) => {
  console.error('[verify-req3] ' + (e && e.stack ? e.stack : String(e)));
  process.exit(1);
});