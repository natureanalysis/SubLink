#!/usr/bin/env node
'use strict';
/**
 * take-screens.cjs —— 给 README / 文档拍界面截图（默认英文，可拍中文）。
 *
 * 原理：界面是纯静态文件（crates/sl-gui/ui/，编译时内嵌进 exe），
 * 用 ?mock=1 挂一套内存假数据即可在普通浏览器里完整渲染；
 * 界面语言默认英文、选择记 localStorage（sublink.locale），
 * 所以拍中文要用 addInitScript 预设这个键（默认英文后不能只靠浏览器 locale）。
 * 不需要启动 Tauri，也不需要 npm / 打包器。
 *
 * 依赖：playwright（npm i playwright，或设 NODE_PATH 指向已安装的位置）。
 * 浏览器：优先用系统已安装的 Edge / Chrome，都没有再退回 playwright 自带 chromium。
 *
 * 用法：
 *   node scripts/take-screens.cjs                    # 拍英文（main/add-rule/settings/progress）
 *   node scripts/take-screens.cjs --lang zh          # 拍中文（覆盖既有截图）
 *   node scripts/take-screens.cjs --out docs/screenshots --lang en
 *   node scripts/take-screens.cjs --check            * 只做漏翻检查，不截图
 */

const fs = require('fs');
const path = require('path');
const url = require('url');

const ROOT = path.resolve(__dirname, '..');
const UI_INDEX = path.join(ROOT, 'crates', 'sl-gui', 'ui', 'index.html');

const args = process.argv.slice(2);
function argOf(name, dflt) {
  const i = args.indexOf('--' + name);
  return i >= 0 && args[i + 1] && !args[i + 1].startsWith('--') ? args[i + 1] : dflt;
}
const LANG = argOf('lang', 'en');
const OUT = path.resolve(ROOT, argOf('out', 'docs/screenshots'));
const CHECK_ONLY = args.includes('--check');
// 高度要够放下全部 mock 规则（9 条，含「系统设置」那条），否则新特性落在
// 表格滚动区外、截图里看不见；但不能放太高 —— 再往下就是「内置模板」条，
// 露一半反而更像截图翻车。892 恰好：9 条规则全露出，模板条还在折线以下。
const VIEWPORT = { width: 1240, height: 892 };

function fail(msg) { console.error('[take-screens] ' + msg); process.exit(1); }

let chromium;
try {
  chromium = require('playwright').chromium;
} catch (e) {
  fail('缺少 playwright：请先 npm i playwright，或用 NODE_PATH 指向已安装目录。\n  ' + e.message);
}

function uiUrl(extra) {
  return url.pathToFileURL(UI_INDEX).href + '?mock=1' + (extra ? '&' + extra : '');
}

async function launch() {
  const base = { headless: true };
  for (const channel of ['msedge', 'chrome', null]) {
    try {
      return await chromium.launch(Object.assign({}, base, channel ? { channel } : {}));
    } catch (e) {
      console.log('[take-screens] ' + (channel || 'bundled chromium') + ' 不可用，尝试下一个…');
    }
  }
  fail('没有可用的浏览器（试过 Edge / Chrome / 自带 chromium）');
}

/** 全树扫汉字文本节点：英文模式下任何残留汉字都说明漏翻。
 *  只匹配汉字（不含全角符号——"＋ 添加规则"这类文案自带全角加号，属误报）；
 *  语言选择器里的"中文"是语言自称，预期存在。 */
async function scanCJK(page) {
  return page.evaluate(() => {
    const bad = [];
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    const re = /[\u3400-\u9fff\uf900-\ufaff]/;
    while (walker.nextNode()) {
      const s = walker.currentNode.nodeValue.trim();
      if (s && re.test(s) && s !== '中文') bad.push(s.length > 60 ? s.slice(0, 60) + '…' : s);
    }
    return bad;
  });
}

(async () => {
  const browser = await launch();
  const context = await browser.newContext({
    locale: LANG === 'zh' ? 'zh-CN' : 'en-US',
    viewport: VIEWPORT,
    deviceScaleFactor: 1,
  });
  // 界面语言由 localStorage(sublink.locale) 决定（无记录默认英文），
  // 中文截图必须预设；英文是默认值，预设一遍也无害，且让两种语言走同一条路径。
  await context.addInitScript((lang) => {
    try { localStorage.setItem('sublink.locale', lang); } catch (e) {}
  }, LANG);
  const page = await context.newPage();
  const suffix = LANG === 'zh' ? '' : '-' + LANG;

  // ---- 主窗口（列表首屏；详情面板已移除，构图以表格为主） ----
  await page.goto(uiUrl(), { waitUntil: 'load' });
  await page.waitForSelector('#rows tr');
  await page.waitForFunction(() => document.querySelectorAll('#rows tr').length >= 8);
  if (CHECK_ONLY) {
    const bad = await scanCJK(page);
    console.log(bad.length ? 'main-window CJK 残留:\n  ' + bad.join('\n  ') : 'main-window: 无 CJK 残留');
  } else {
    const h = await page.evaluate(() => document.documentElement.scrollHeight);
    await page.setViewportSize({ width: VIEWPORT.width, height: Math.min(h + 2, 1200) });
    await page.screenshot({ path: path.join(OUT, 'main-window' + suffix + '.png') });
    await page.setViewportSize(VIEWPORT);
    console.log('main-window' + suffix + '.png  (' + VIEWPORT.width + 'x' + Math.min(h + 2, 1200) + ')');
  }

  // ---- 添加规则草稿行（目标子目录填上与中文截图相同的值） ----
  await page.click('#btn-add');
  await page.waitForSelector('#rows tr.draft-row');
  await page.fill('#rows tr.draft-row [data-draft-field="name"] input', 'Chrome cache');
  await page.fill('#rows tr.draft-row [data-draft-field="source"] input', '%LOCALAPPDATA%\\Google\\Chrome\\User Data\\Default\\Cache');
  await page.fill('#rows tr.draft-row [data-draft-field="target"] input', 'browsers\\chrome-cache');
  await page.waitForTimeout(120);
  if (CHECK_ONLY) {
    const bad = await scanCJK(page);
    console.log(bad.length ? 'add-rule CJK 残留:\n  ' + bad.join('\n  ') : 'add-rule: 无 CJK 残留');
  } else {
    await page.screenshot({ path: path.join(OUT, 'add-rule' + suffix + '.png') });
    console.log('add-rule' + suffix + '.png');
  }

  // ---- 设置弹窗 ----
  await page.goto(uiUrl('settings=1'), { waitUntil: 'load' });
  await page.waitForSelector('#modal:not(.hidden)');
  await page.waitForTimeout(200);
  if (CHECK_ONLY) {
    const bad = await scanCJK(page);
    console.log(bad.length ? 'settings CJK 残留:\n  ' + bad.join('\n  ') : 'settings: 无 CJK 残留');
  } else {
    await page.screenshot({ path: path.join(OUT, 'settings' + suffix + '.png') });
    console.log('settings' + suffix + '.png');
  }

  // ---- 批量应用进度浮层 ----
  await page.goto(uiUrl('progress=1'), { waitUntil: 'load' });
  await page.waitForSelector('#progress:not(.hidden)');
  await page.waitForTimeout(150);
  if (CHECK_ONLY) {
    const bad = await scanCJK(page);
    console.log(bad.length ? 'progress CJK 残留:\n  ' + bad.join('\n  ') : 'progress: 无 CJK 残留');
  } else {
    await page.screenshot({ path: path.join(OUT, 'progress' + suffix + '.png') });
    console.log('progress' + suffix + '.png');
  }

  await browser.close();
})().catch((e) => fail(e && e.stack ? e.stack : String(e)));
