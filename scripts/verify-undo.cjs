#!/usr/bin/env node
'use strict';
/**
 * verify-undo.cjs — 「撤销」按钮的端到端验证（走 mock=1 界面）。
 *
 * 覆盖：
 *   1) 工具栏常驻「撤销」按钮，回收站为空时置灰；
 *   2) 删除规则后按钮变亮，列表里少了一条；
 *   3) 点「撤销」把规则原样搬回来（id 不变），列表恢复；
 *   4) 撤销后按钮重新置灰；
 *   5) 删除多条时按批撤销：一批一起回来。
 *
 * 用法：node scripts/verify-undo.cjs
 * 依赖：playwright（见 take-screens.cjs 的 NODE_PATH 说明）
 */

const path = require('path');
const url = require('url');

const ROOT = path.resolve(__dirname, '..');
const UI_INDEX = path.join(ROOT, 'crates', 'sl-gui', 'ui', 'index.html');

let chromium;
try {
  chromium = require('playwright').chromium;
} catch (e) {
  console.error('[verify-undo] 缺少 playwright：' + e.message);
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
      return await chromium.launch(Object.assign({ headless: true }, channel ? { channel: channel } : {}));
    } catch (e) { /* 试下一个 */ }
  }
  throw new Error('没有可用的浏览器（试过 Edge / Chrome / 自带 chromium）');
}

/** 当前列表里的规则 id 与名称。 */
async function rows(page) {
  return page.evaluate(() => {
    return Array.from(document.querySelectorAll('#rows tr[data-id]')).map(function (tr) {
      const nameCell = tr.querySelector('td[data-field="name"]');
      return {
        id: tr.getAttribute('data-id'),
        name: nameCell ? nameCell.textContent.trim() : '',
      };
    });
  });
}

/** 「撤销」按钮的可用态与文案。 */
async function undoBtn(page) {
  return page.evaluate(() => {
    const b = document.getElementById('btn-undo');
    return b
      ? { exists: true, disabled: b.disabled, title: b.getAttribute('title') || '', text: b.textContent.trim() }
      : { exists: false };
  });
}

/** 勾选前 N 条规则（勾选框在第一列）。 */
async function pick(page, n) {
  await page.evaluate((count) => {
    const boxes = Array.from(document.querySelectorAll('#rows tr[data-id] input[type="checkbox"][data-act="pick"]'));
    boxes.slice(0, count).forEach(function (b) { if (!b.checked) b.click(); });
  }, n);
  await page.waitForTimeout(100);
}

/** 走工具栏「删除」并确认（会弹确认框）。 */
async function deletePicked(page) {
  await page.click('#btn-delete');
  await page.waitForSelector('#modal:not(.hidden)');
  await page.evaluate(() => {
    const btns = Array.from(document.querySelectorAll('#modal-foot button'));
    const del = btns.find(function (b) { return b.textContent.indexOf('删除') >= 0; });
    if (del) del.click();
  });
  await page.waitForSelector('#modal.hidden');
  await page.waitForTimeout(200);
}

(async () => {
  const browser = await launch();
  const context = await browser.newContext({ locale: 'zh-CN', viewport: { width: 1600, height: 900 } });
  await context.addInitScript(() => {
    try { localStorage.setItem('sublink.locale', 'zh'); } catch (e) {}
  });
  const page = await context.newPage();
  const errors = [];
  page.on('console', function (m) { if (m.type() === 'error') errors.push(m.text()); });
  page.on('pageerror', function (e) { errors.push(String(e)); });

  const uiUrl = url.pathToFileURL(UI_INDEX).href + '?mock=1';
  await page.goto(uiUrl, { waitUntil: 'load' });
  await page.waitForSelector('#rows tr[data-id]');
  await page.waitForFunction(() => document.querySelectorAll('#rows tr[data-id]').length >= 9);

  // ---- 1) 按钮常驻，回收站为空时置灰 ----
  const b0 = await undoBtn(page);
  ok('工具栏有常驻的「撤销」按钮', b0.exists === true, JSON.stringify(b0));
  ok('回收站为空时按钮置灰', b0.exists && b0.disabled === true, 'disabled=' + (b0 && b0.disabled));

  const before = await rows(page);
  ok('初始列表非空', before.length > 0, 'rows=' + before.length);

  // ---- 2) 删一条：按钮变亮，列表少一条 ----
  await pick(page, 1);
  const target = before[0];
  await deletePicked(page);
  const afterDel = await rows(page);
  ok('删除后列表少了一条', afterDel.length === before.length - 1,
    before.length + ' → ' + afterDel.length);
  ok('被删的规则确实不在列表里',
    !afterDel.some(function (r) { return r.id === target.id; }), 'id=' + target.id);

  const b1 = await undoBtn(page);
  ok('有可撤销内容时按钮可用', b1.exists && b1.disabled === false, 'disabled=' + (b1 && b1.disabled));
  ok('按钮提示里点名了被删的规则', b1.exists && b1.title.indexOf(target.name) >= 0,
    'title=' + b1.title);

  // ---- 3) 撤销：原样回来（id 不变） ----
  await page.click('#btn-undo');
  await page.waitForTimeout(300);
  const afterUndo = await rows(page);
  ok('撤销后列表条数恢复', afterUndo.length === before.length,
    afterDel.length + ' → ' + afterUndo.length);
  const back = afterUndo.find(function (r) { return r.id === target.id; });
  ok('撤销后原规则按原 id 回来', !!back, 'id=' + target.id + ' name=' + (back ? back.name : 'n/a'));
  ok('撤销后规则名称不变', !!back && back.name === target.name,
    (back ? back.name : 'n/a') + ' vs ' + target.name);

  // ---- 4) 撤销后按钮重新置灰 ----
  const b2 = await undoBtn(page);
  ok('撤销后按钮重新置灰', b2.exists && b2.disabled === true, 'disabled=' + (b2 && b2.disabled));

  // ---- 5) 批量删除：按批一起回来 ----
  await pick(page, 3);
  const beforeBatch = await rows(page);
  await deletePicked(page);
  const afterBatchDel = await rows(page);
  ok('批量删除后少了三条', afterBatchDel.length === beforeBatch.length - 3,
    beforeBatch.length + ' → ' + afterBatchDel.length);

  await page.click('#btn-undo');
  await page.waitForTimeout(300);
  const afterBatchUndo = await rows(page);
  ok('撤销后三条一起回来', afterBatchUndo.length === beforeBatch.length,
    afterBatchDel.length + ' → ' + afterBatchUndo.length);

  ok('全程没有控制台错误', errors.length === 0, errors.join(' | '));

  await browser.close();

  const failed = checks.filter(function (c) { return !c.pass; });
  console.log('\n' + (checks.length - failed.length) + '/' + checks.length + ' 项通过');
  if (failed.length) {
    console.log('失败项：');
    failed.forEach(function (f) { console.log('  - ' + f.name + (f.detail ? '  → ' + f.detail : '')); });
    process.exit(1);
  }
})().catch(function (e) {
  console.error('[verify-undo] 运行失败：' + (e && e.stack ? e.stack : e));
  process.exit(1);
});
