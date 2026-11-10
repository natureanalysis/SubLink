#!/usr/bin/env node
'use strict';
/**
 * verify-req4.cjs —— 清理时按类型排除文件的界面取证（需求 4）。
 *
 * 用 ?mock=1 起界面，程序化测量 + 抓 invoke 实参：
 *   1) 清理弹窗里有「保留类型」输入框（文案到位、宽度可用、不被挤扁）；
 *   2) 空类型预演 → 提示里的 excluded 计数为 mock 基线 12；
 *   3) 填 .py 后预演 → 计数变 17，且 clean_preview 的 typeExcludes 实参正确；
 *   4) 输入框内容/策略单选变化后，预演结果作废、删除按钮禁用（防"看到的≠删掉的"）；
 *   5) 重新预演后按钮恢复可用；点删除时 clean_run 带上了同一份 typeExcludes。
 *
 * 用法： node scripts/verify-req4.cjs
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
  console.error('[verify-req4] 缺少 playwright：' + e.message);
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

// 弹窗体内有两个 .field-row（时间段输入 / 按钮行），按钮行靠 danger 按钮唯一定位。
const BTNROW = '#modal-body .field-row:has(button.danger)';
const PREVIEW_BTN = BTNROW + ' button:not(.danger)';
const KEEP_SEL = '#modal-body input[type="text"]';

const hostText = (page) => page.evaluate((sel) =>
  document.querySelector(sel).previousElementSibling.textContent, BTNROW);
const delDisabled = (page) => page.evaluate((sel) =>
  document.querySelector(sel + ' button.danger').disabled, BTNROW);
const delText = (page) => page.evaluate((sel) =>
  document.querySelector(sel + ' button.danger').textContent, BTNROW);
const calls = (page) => page.evaluate(() => window.__req4 || []);
const waitCount = (page, n) => page.waitForFunction((arg) =>
  document.querySelector(arg.sel).previousElementSibling.textContent.indexOf(arg.n) >= 0,
  { sel: BTNROW, n: n });

(async () => {
  const browser = await launch();
  const context = await browser.newContext({ locale: 'zh-CN', viewport: { width: 1240, height: 900 } });
  await context.addInitScript(() => {
    try { localStorage.setItem('sublink.locale', 'zh'); } catch (e) {}
  });
  const page = await context.newPage();
  const uiUrl = url.pathToFileURL(UI_INDEX).href + '?mock=1';

  await page.goto(uiUrl, { waitUntil: 'load' });
  await page.waitForSelector('#rows tr');
  await page.waitForFunction(() => document.querySelectorAll('#rows tr').length >= 8);

  // 抓 invoke 实参（mock 后端对象上的方法做一层包装，应用内部走的就是它）
  const hooked = await page.evaluate(() => {
    window.__req4 = [];
    try {
      const orig = backend.invoke;
      backend.invoke = function (n, a) {
        window.__req4.push({ n: n, a: a });
        return orig.call(backend, n, a);
      };
      return true;
    } catch (e) {
      return 'hook failed: ' + e.message;
    }
  });
  ok('可以监听 mock 后端 invoke（前置条件）', hooked === true, String(hooked));

  // ---- 打开清理弹窗（详情面板已移除，走行内「清理」按钮） ----
  await page.click('#rows tr[data-id="1"] [data-act="clean"]');
  await page.waitForSelector('#modal:not(.hidden) ' + BTNROW);

  // ---- 1) 保留类型控件 ----
  const keepInfo = await page.evaluate(() => {
    const body = document.querySelector('#modal-body');
    const input = body.querySelector('input[type="text"]');
    if (!input) return null;
    const wrap = input.parentElement;
    const label = wrap.querySelector('label');
    const hint = wrap.querySelector('.hint');
    const rect = input.getBoundingClientRect();
    return {
      label: label ? label.textContent : '',
      ph: input.placeholder,
      hint: hint ? hint.textContent : '',
      w: Math.round(rect.width),
      scroll: input.scrollWidth,
      client: input.clientWidth,
    };
  });
  ok('清理弹窗有「保留类型」输入框', !!keepInfo && keepInfo.label.length > 0,
    keepInfo ? 'label=' + keepInfo.label : 'not found');
  ok('输入框宽度可用（>=120px 且不截断）',
    !!keepInfo && keepInfo.w >= 120 && keepInfo.scroll <= keepInfo.client + 1,
    keepInfo ? ('w=' + keepInfo.w + ' scroll=' + keepInfo.scroll + ' client=' + keepInfo.client) : 'n/a');
  ok('输入框有占位与说明文案', !!keepInfo && keepInfo.ph.length > 0 && keepInfo.hint.length > 0,
    keepInfo ? ('ph=' + keepInfo.ph + ' | hint=' + keepInfo.hint) : 'n/a');

  // ---- 2) 空类型预演 ----
  await page.click(PREVIEW_BTN);
  await waitCount(page, '12');
  const t0 = await hostText(page);
  ok('空类型时 excluded 计数为 mock 基线 12', t0.indexOf('12') >= 0, 'text=' + t0.replace(/\s+/g, ' ').trim());
  const delLabel = await delText(page);
  ok('预演后删除按钮可用', !(await delDisabled(page)), 'del=' + JSON.stringify(delLabel));

  const pv0 = (await calls(page)).filter((c) => c.n === 'clean_preview').pop();
  ok('空类型时 clean_preview 传了空 typeExcludes',
    !!pv0 && Array.isArray(pv0.a.typeExcludes) && pv0.a.typeExcludes.length === 0,
    pv0 ? JSON.stringify(pv0.a.typeExcludes) : 'no call');

  // ---- 3) 填 .py 后预演 ----
  await page.fill(KEEP_SEL, '.py');
  await page.click(PREVIEW_BTN);
  await waitCount(page, '17');
  const t1 = await hostText(page);
  ok('填 .py 后 excluded 计数变 17', t1.indexOf('17') >= 0, 'text=' + t1.replace(/\s+/g, ' ').trim());
  const pv1 = (await calls(page)).filter((c) => c.n === 'clean_preview').pop();
  ok('clean_preview 的 typeExcludes 实参为 [".py"]',
    !!pv1 && JSON.stringify(pv1.a.typeExcludes) === JSON.stringify(['.py']),
    pv1 ? JSON.stringify(pv1.a.typeExcludes) : 'no call');

  // ---- 4) 改类型 → 预演作废 ----
  await page.fill(KEEP_SEL, '.py, .log');
  await page.waitForTimeout(120);
  const tClear = await hostText(page);
  ok('改保留类型后预演结果被清空', tClear === '', 'text=' + JSON.stringify(tClear));
  const dDis = await delDisabled(page);
  ok('改保留类型后删除按钮禁用', dDis === true, 'disabled=' + String(dDis));

  // ---- 5) 重新预演 + 改策略也作废 ----
  await page.click(PREVIEW_BTN);
  await waitCount(page, '17');
  const dUse = await delDisabled(page);
  ok('重新预演后删除按钮恢复可用', dUse === false, 'disabled=' + String(dUse));
  await page.click('#modal-body input[type="radio"][value="before_hour"]');
  await page.waitForTimeout(120);
  const tClear2 = await hostText(page);
  ok('改策略后预演结果同样被清空', tClear2 === '', 'text=' + JSON.stringify(tClear2));

  // ---- 6) 删除时带上同一份 typeExcludes ----
  await page.click(PREVIEW_BTN);
  await waitCount(page, '17');
  await page.click(BTNROW + ' button.danger');
  await page.waitForFunction(() => document.querySelector('#modal').classList.contains('hidden'));
  const runCall = (await calls(page)).filter((c) => c.n === 'clean_run').pop();
  ok('clean_run 的 typeExcludes 实参为 [".py", ".log"]',
    !!runCall && JSON.stringify(runCall.a.typeExcludes) === JSON.stringify(['.py', '.log']),
    runCall ? JSON.stringify(runCall.a.typeExcludes) : 'no call');
  const toastText = await page.evaluate(() => {
    const el = document.querySelector('#toasts');
    return el ? el.textContent : '';
  });
  ok('删除后提示正常给出（未抛错）', typeof toastText === 'string' && toastText.length > 0,
    'toast=' + toastText.replace(/\s+/g, ' ').trim());

  await browser.close();

  const failed = checks.filter((c) => !c.pass);
  console.log('\n[verify-req4] ' + (checks.length - failed.length) + '/' + checks.length + ' passed');
  if (failed.length) {
    console.log('FAILED: ' + failed.map((c) => c.name).join(' | '));
    process.exit(1);
  }
  console.log('VERIFY_PASS');
})().catch((e) => {
  console.error('[verify-req4] 运行异常：' + (e && e.stack ? e.stack : e));
  process.exit(2);
});