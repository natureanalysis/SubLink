#!/usr/bin/env node
'use strict';
/**
 * verify-req5.cjs —— 隐藏态随配置导出导入的界面取证（需求 5）。
 *
 * 用 ?mock=1 起界面，patch 掉 pick_open_file，直接跑 doImport()：
 *   1) 导入提示里出现「隐藏 N 条」（zh / en 各一遍，且键在两种语言里都有、未回退成键名）；
 *   2) mock 后端的 import_config 返回值带 rules_hidden（前后端契约对齐）；
 *   3) 导入后列表仍正常刷新（refresh 走通）。
 *
 * 用法： node scripts/verify-req5.cjs
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
  console.error('[verify-req5] 缺少 playwright：' + e.message);
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

const uiUrl = url.pathToFileURL(UI_INDEX).href + '?mock=1';

async function run(browser, locale) {
  const context = await browser.newContext({ locale: locale === 'zh' ? 'zh-CN' : 'en-US', viewport: { width: 1240, height: 900 } });
  await context.addInitScript((loc) => {
    try { localStorage.setItem('sublink.locale', loc); } catch (e) {}
  }, locale);
  const page = await context.newPage();
  await page.goto(uiUrl, { waitUntil: 'load' });
  await page.waitForSelector('#rows tr');

  await page.evaluate(() => {
    window.__req5 = [];
    const orig = backend.invoke;
    backend.invoke = function (n, a) {
      window.__req5.push({ n: n, a: a });
      if (n === 'pick_open_file') return Promise.resolve('C:/tmp/req5.json');
      return orig.call(backend, n, a);
    };
  });

  const mockRep = await page.evaluate(() => backend.invoke('import_config', { path: 'x', ramRoot: null, dryRun: false }));

  await page.evaluate(() => doImport());
  await page.waitForSelector('#toasts .toast');

  const info = await page.evaluate(() => {
    const box = document.querySelector('#toasts .toast');
    return {
      title: box.querySelector('.toast-title').textContent,
      body: box.textContent,
      calls: window.__req5.map(function (c) { return c.n; }),
      rows: document.querySelectorAll('#rows tr').length,
      dictZh: DICT.zh['io.import_hidden'],
      dictEn: DICT.en['io.import_hidden'],
      lang: getLang(),
    };
  });
  await context.close();
  return { info: info, mockRep: mockRep };
}

(async () => {
  const browser = await launch();

  // ---------------- 中文 ----------------
  const zh = await run(browser, 'zh');
  console.log('  [zh] toast = ' + JSON.stringify(zh.info.body));
  ok('zh：语言确为中文', zh.info.lang === 'zh', String(zh.info.lang));
  ok('zh：导入提示标题正确', zh.info.title === '导入完成', zh.info.title);
  ok('zh：提示里带「隐藏 2 条」', zh.info.body.indexOf('隐藏 2 条') >= 0, zh.info.body);
  ok('zh：新建/跳过口径未变', zh.info.body.indexOf('规则 5') >= 0 && zh.info.body.indexOf('跳过 2') >= 0, zh.info.body);
  ok('zh：问题条数仍在', zh.info.body.indexOf('问题 1 条') >= 0, zh.info.body);
  ok('zh：没有把键名当文案（漏翻回退）', zh.info.body.indexOf('io.import_hidden') < 0, zh.info.body);
  ok('zh：确实调用了 import_config', zh.info.calls.indexOf('import_config') >= 0, zh.info.calls.join(','));
  ok('zh：导入后列表仍刷新出条目', zh.info.rows > 0, 'rows=' + zh.info.rows);
  ok('zh：i18n 两种语言都有 io.import_hidden', !!zh.info.dictZh && !!zh.info.dictEn, JSON.stringify([zh.info.dictZh, zh.info.dictEn]));
  ok('mock 后端 import_config 返回 rules_hidden', zh.mockRep && zh.mockRep.rules_hidden === 2, JSON.stringify(zh.mockRep));

  // ---------------- 英文 ----------------
  const en = await run(browser, 'en');
  console.log('  [en] toast = ' + JSON.stringify(en.info.body));
  ok('en：语言确为英文', en.info.lang === 'en', String(en.info.lang));
  ok('en：导入提示标题正确', en.info.title === 'Import complete', en.info.title);
  ok('en：提示里带 ", 2 hidden"', en.info.body.indexOf(', 2 hidden') >= 0, en.info.body);
  ok('en：没有把键名当文案（漏翻回退）', en.info.body.indexOf('io.import_hidden') < 0, en.info.body);

  await browser.close();

  const failed = checks.filter(function (c) { return !c.pass; });
  console.log('\n[verify-req5] ' + (failed.length ? 'VERIFY_FAIL ' + failed.length + '/' + checks.length : 'VERIFY_PASS ' + checks.length + '/' + checks.length));
  process.exit(failed.length ? 1 : 0);
})().catch(function (e) {
  console.error('[verify-req5] 异常：' + (e && e.stack ? e.stack : e));
  process.exit(1);
});