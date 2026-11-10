/* SubLink 前端逻辑。
 *
 * 设计约束（都是踩过的坑或明确的取舍）：
 *  - 不引 npm / 打包器：这是一张表格 + 几个弹窗，前端工具链只会带来维护负担。
 *    所以走 `withGlobalTauri`，用 window.__TAURI__.core.invoke 调命令。
 *  - CSP 是 `style-src 'self'`：**不写内联 style 属性、不注入 <style>**，
 *    需要动态样式时只走 CSSOM（el.style.x = y）。
 *  - 弹窗/详情面板都用事件委托（data-act + data-id），避免每次重渲染都重新绑定。
 *  - `?mock=1` 会挂上一套内存假数据（本地打开 index.html 即可看/调界面），
 *    用于无头浏览器截图核验；`&progress=1` 额外展示进度浮层。
 */
'use strict';

// ---------------------------------------------------------------------------
// 后端桥
// ---------------------------------------------------------------------------

const PARAMS = new URLSearchParams(location.search);
const MOCK = PARAMS.has('mock');

function tauriInvoke(name, args) {
  const g = window.__TAURI__;
  if (g && g.core && typeof g.core.invoke === 'function') return g.core.invoke(name, args || {});
  const internals = window.__TAURI_INTERNALS__;
  if (internals && typeof internals.invoke === 'function') return internals.invoke(name, args || {});
  return Promise.reject(new Error(t('err.no_bridge')));
}

function tauriListen(event, handler) {
  const g = window.__TAURI__;
  if (g && g.event && typeof g.event.listen === 'function') return g.event.listen(event, handler);
  return Promise.resolve(function () {});
}

const backend = MOCK ? createMockBackend() : {
  invoke: tauriInvoke,
  listen: tauriListen,
};

function invoke(name, args) {
  return backend.invoke(name, args).catch(function (err) {
    const msg = typeof err === 'string' ? err : (err && err.message) || String(err);
    throw new Error(msg);
  });
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

// 列显示偏好：只允许隐藏低频列，键名同时用于 table.rules 的 class 和 localStorage。
const HIDDEN_COLS_KEY = 'sublink.hiddenCols';
const COL_KEYS = ['link', 'auto', 'limit', 'excludes', 'notes'];
const COL_WIDTHS = { link: 76, auto: 150, limit: 112, excludes: 80, notes: 110 };

const state = {
  snap: null,
  picked: new Set(),
  // 内置模板（preset_rules 的返回，后端已按"本机存在源目录"过滤）。
  // 还没建成本地规则的模板会作为"未添加"行直接显示在主列表里。
  presets: [],
  // 已勾选但还没建成规则的内置模板（键 = source_template 小写）。
  pickedTpl: new Set(),
  focusId: null,
  // showHidden：把被隐藏的条目也显示出来（默认关）。隐藏态由后端存在 settings 里
  // （见 usecase::rules::KEY_HIDDEN_RULES），所以这里只是个"要不要看"的开关。
  filters: { q: '', category: '', onlyEnabled: false, onlyAttention: false, showHidden: false },
  busy: false,
  drag: null,
  // 表头点击排序：key=null 表示用后端返回顺序（category, sort_order, id）。
  // 客户端排序只影响展示，不写库 —— 拖拽排序（sort_order）才是持久顺序。
  sort: { key: null, dir: 'asc' },
  // 「空间统计」的扫描结果：rule_id -> DiskUsageEntry（bytes / files / bytes_text）。
  // 只有点过「空间统计」才有值；为 null 时表格该列显示占位符。
  diskUsage: null,
  // 分类树侧栏：展开的分类名集合；treeSeeded 保证首屏只自动展开一次当前条目所在分类。
  treeOpen: new Set(),
  treeSeeded: false,
  // 列表顶部的新规则草稿（null = 没有草稿行）。新增规则不再弹表单，直接在
  // 列表行里填；保存后才写入后端，取消就丢掉这一行。
  draft: null,
  // 被隐藏的表格列（localStorage 持久化）。只影响展示，不影响任何后端行为。
  hiddenCols: loadHiddenCols(),
  // 界面偏好（后端 settings 表持久化）：关闭窗口是否驻留托盘、
  // 应用时是否允许合并到已有目标目录。缺省值 = 安全的那一侧。
  uiSettings: { close_to_tray: true, allow_merge: false },
};

// 分类标签走 i18n（键 cat.<category>）；未收录的分类回退原始值。
function categoryLabel(c) {
  if (!c) return c;
  const label = t('cat.' + c);
  return label === 'cat.' + c ? c : label;
}

// ---------------------------------------------------------------------------
// 列显示偏好（localStorage；只影响前端展示）
// ---------------------------------------------------------------------------

function loadHiddenCols() {
  try {
    const raw = localStorage.getItem(HIDDEN_COLS_KEY);
    if (!raw) return new Set();
    const arr = JSON.parse(raw);
    if (!Array.isArray(arr)) return new Set();
    return new Set(arr.filter(function (k) { return COL_KEYS.indexOf(k) >= 0; }));
  } catch (e) {
    return new Set();
  }
}

function saveHiddenCols() {
  try {
    localStorage.setItem(HIDDEN_COLS_KEY, JSON.stringify(Array.from(state.hiddenCols)));
  } catch (e) { /* 隐私模式 / 配额满：退化为本次会话生效 */ }
}

/** 同步 table.rules 的 class、最小宽度和「更多」菜单里的勾选态。 */
function applyColumnVisibility() {
  const table = document.querySelector('table.rules');
  if (!table) return;
  let min = 1560;
  COL_KEYS.forEach(function (key) {
    const hidden = state.hiddenCols.has(key);
    table.classList.toggle('hide-col-' + key, hidden);
    if (hidden) min -= COL_WIDTHS[key];
  });
  table.style.minWidth = Math.max(900, min) + 'px';
  document.querySelectorAll('#more-menu input[data-col]').forEach(function (cb) {
    cb.checked = !state.hiddenCols.has(cb.dataset.col);
  });
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

function $(id) { return document.getElementById(id); }

function esc(s) {
  return String(s === null || s === undefined ? '' : s).replace(/[&<>"']/g, function (c) {
    return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
  });
}

function humanBytes(n) {
  if (!n || n <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = n, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return i === 0 ? n + ' B' : v.toFixed(1) + ' ' + units[i];
}

function badgeClass(badge) {
  if (badge === '✅') return 'ok';
  if (badge === '⚠️') return 'warn';
  if (badge === '🚫') return 'foreign';
  return 'plain';
}


function toast(title, message, kind) {
  const box = document.createElement('div');
  box.className = 'toast ' + (kind || '');
  const titleEl = document.createElement('div');
  titleEl.className = 'toast-title';
  titleEl.textContent = title;
  const m = document.createElement('div');
  m.textContent = message || '';
  box.appendChild(titleEl);
  if (message) box.appendChild(m);
  $('toasts').appendChild(box);
  setTimeout(function () { box.remove(); }, kind === 'error' ? 9000 : 4500);
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast(t('common.copied'), text.length > 60 ? text.slice(0, 60) + '…' : text, 'ok');
  } catch (e) {
    toast(t('common.copy_failed'), t('common.copy_manual'), 'warn');
  }
}

// ---------------------------------------------------------------------------
// 数据加载
// ---------------------------------------------------------------------------

async function refresh(keepFocus) {
  try {
    state.snap = await invoke('ui_snapshot');
    const alive = state.snap.rules.some(function (r) { return r.id === state.focusId; });
    if (!keepFocus || !alive) state.focusId = state.snap.rules.length ? state.snap.rules[0].id : null;
    // 内置模板：后端已按"本机是否存在该源目录"过滤，模板源目录不存在的机型自然拿不到条目。
    // 命令不存在（旧后端）时静默降级为"没有模板"，不影响主列表。
    try { state.presets = await invoke('preset_rules', {}); } catch (e) { state.presets = []; }
    // 界面偏好：决定"应用"时能不能合并到已有目标目录。读不到就用默认值
    // （不合并）—— 宁可少做，也不覆盖用户文件。
    try { state.uiSettings = await invoke('ui_settings', {}); } catch (e) { /* 用默认值 */ }
    // 清掉已经不存在的勾选项
    const ids = new Set(state.snap.rules.map(function (r) { return r.id; }));
    state.picked.forEach(function (id) { if (!ids.has(id)) state.picked.delete(id); });
    pruneTplPicks();
    render();
  } catch (e) {
    toast(t('toast.load_failed'), e.message, 'error');
  }
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

function render() {
  renderBanners();
  renderHeader();
  renderFilterOptions();
  renderSidebar();
  renderRows();
  applyColumnVisibility();
  renderUndoButton();

  renderSortIndicators();
}

/**
 * 「撤销」按钮的可用态：撤销栈为空就置灰。
 *
 * 按钮**常驻**在工具栏（不是只在有东西可撤销时才出现）——
 * 位置固定，用户不必去找；亮灭告诉它当前有没有可撤销的操作。
 *
 * 悬停提示要说清"这一下会撤什么"：栈顶可能是删除（规则被搬走了），
 * 也可能是修改（字段被改了）—— 两者对用户的含义完全不同。
 */
function renderUndoButton() {
  const btn = $('btn-undo');
  if (!btn) return;
  const undo = (state.snap && state.snap.undo) || { pending: 0, kind: '', names: [] };
  const pending = undo.pending || 0;
  btn.disabled = pending === 0;
  if (pending === 0) {
    btn.setAttribute('title', t('undo.disabled_tip'));
  } else {
    const names = (undo.names || []).join(getLang() === 'zh' ? '、' : ', ');
    const tip = undo.kind === 'update' ? t('undo.tip_update') : t('undo.tip_delete');
    // 分隔符跟着界面语言走：中文用全角冒号，英文用半角 + 空格。
    const sep = getLang() === 'zh' ? '：' : ': ';
    btn.setAttribute('title', tip + (names ? sep + names : ''));
  }
}

function renderBanners() {
  const host = $('banners');
  host.textContent = '';
  (state.snap.banners || []).forEach(function (b) {
    const d = document.createElement('div');
    d.className = 'banner ' + b.level;
    const dot = document.createElement('span');
    dot.className = 'dot';
    const txt = document.createElement('span');
    txt.textContent = b.text;
    d.appendChild(dot);
    d.appendChild(txt);
    if (b.action === 'sweep') {
      const btn = document.createElement('button');
      btn.className = 'banner-action';
      btn.textContent = t('banner.clean_now');
      btn.addEventListener('click', openBatchCleanDialog);
      d.appendChild(btn);
    }
    host.appendChild(d);
  });
}

function renderHeader() {
  const roots = state.snap.roots || [];
  const ready = roots.filter(function (r) { return r.state === 'ready'; });
  let sub;
  if (!roots.length) {
    sub = t('header.no_root');
  } else {
    const first = ready[0] || roots[0];
    const mark = first.state === 'ready'
      ? t('header.root_ready')
      : tf('header.root_unavailable', { msg: first.message });
    sub = tf('header.root_prefix', { path: first.path, mark: mark });
    if (ready.length && first.message && first.state === 'ready') sub += ' · ' + first.message;
    if (roots.length > 1) sub += tf('header.roots_more', { n: roots.length });
  }
  $('root-summary').textContent = sub;

  const s = state.snap.stats;
  $('stat-summary').textContent =
    tf('stat.summary', { total: s.total, enabled: s.enabled, ok: s.ok, warn: s.warn, plain: s.plain }) +
    (s.other ? ' ❔ ' + s.other : '');
}

function renderFilterOptions() {
  const sel = $('filter-category');
  if (sel.options.length === 0) {
    const all = document.createElement('option');
    all.value = '';
    all.textContent = t('filter.all_categories');
    sel.appendChild(all);
    (state.snap.categories || []).forEach(function (c) {
      const o = document.createElement('option');
      o.value = c;
      o.textContent = categoryLabel(c);
      sel.appendChild(o);
    });
  }
  sel.value = state.filters.category;
}

// ---------------------------------------------------------------------------
// 分类树侧栏
//
// 条目多起来以后分类下拉不够用：侧栏画成「全部规则 → 分类 → 规则名」两层树，
// 点分类等于套用分类筛选（与下拉框同一份 state.filters.category），
// 点规则名则切到该分类并把对应行滚到视野中间。
// ---------------------------------------------------------------------------

const SIDEBAR_KEY = 'sublink.sidebar';

/** 树里的一行（按钮）：根 / 分类（可展开，带计数与需处理圆点）/ 规则名。 */
function treeRow(opts) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'tree-row' + (opts.depth ? ' child' : '') + (opts.active ? ' active' : '');
  // CSP 只允许 CSSOM 改样式，内联 style 属性会被拦；层级缩进走这里。
  btn.style.paddingLeft = (8 + opts.depth * 14) + 'px';
  btn.dataset.act = opts.act;
  if (opts.cat !== undefined) btn.dataset.cat = opts.cat;
  if (opts.id !== undefined) btn.dataset.id = String(opts.id);

  const caret = document.createElement('span');
  caret.className = 'tree-caret';
  caret.textContent = opts.children ? (opts.open ? '▾' : '▸') : '';
  btn.appendChild(caret);

  const label = document.createElement('span');
  label.className = 'tree-label';
  label.textContent = opts.label;
  btn.appendChild(label);

  if (opts.attention) {
    const dot = document.createElement('span');
    dot.className = 'tree-dot';
    dot.title = t('sidebar.attention_tip');
    btn.appendChild(dot);
  }

  const count = document.createElement('span');
  count.className = 'tree-count';
  if (opts.count) count.textContent = String(opts.count);
  btn.appendChild(count);
  // 规则节点整行可拖：同一分类内拖动即重排（顺序写库）。
  if (opts.act === 'rule') btn.title = t('row.drag_tip');
  return btn;
}

function renderSidebar() {
  const host = $('tree');
  if (!host || !state.snap) return;
  const all = state.snap.rules || [];
  // 隐藏条目按当前的「显示已隐藏」开关决定是否出现在树里，计数跟着一致。
  const shown = all.filter(function (r) { return !r.hidden || state.filters.showHidden; });

  if (!state.treeSeeded) {
    state.treeSeeded = true;
    const focused = shown.find(function (r) { return r.id === state.focusId; });
    if (focused) state.treeOpen.add(focused.category);
  }

  const byCat = {};
  shown.forEach(function (r) { (byCat[r.category] = byCat[r.category] || []).push(r); });

  host.textContent = '';
  host.appendChild(treeRow({
    depth: 0,
    label: t('sidebar.all'),
    count: shown.length,
    act: 'all',
    active: !state.filters.category,
  }));

  (state.snap.categories || []).forEach(function (c) {
    const items = byCat[c] || [];
    // 空分类不占位置；但当前筛选的分类要留着，否则选中态在树里会消失。
    if (!items.length && state.filters.category !== c) return;
    const open = state.treeOpen.has(c);
    host.appendChild(treeRow({
      depth: 0,
      label: categoryLabel(c),
      count: items.length,
      act: 'cat',
      cat: c,
      children: items.length > 0,
      open: open,
      active: state.filters.category === c,
      attention: items.some(function (r) { return r.needs_attention; }),
    }));
    if (!open) return;
    items.forEach(function (r) {
      host.appendChild(treeRow({
        depth: 1,
        label: r.name,
        count: 0,
        act: 'rule',
        id: r.id,
        active: r.id === state.focusId,
        attention: r.needs_attention,
      }));
    });
  });
}

/** 套用分类筛选（侧栏与下拉框共用）。 */
function applyCategory(c) {
  state.filters.category = c;
  const sel = $('filter-category');
  if (sel) sel.value = c;
  renderSidebar();
  renderRows();
}

function onTreeClick(ev) {
  const btn = ev.target.closest ? ev.target.closest('.tree-row') : null;
  if (!btn) return;
  const act = btn.dataset.act;
  if (act === 'all') {
    applyCategory('');
    return;
  }
  if (act === 'cat') {
    const c = btn.dataset.cat || '';
    if (state.filters.category === c) {
      // 已选中的分类：再点一次只折叠 / 展开，不改变筛选。
      if (state.treeOpen.has(c)) state.treeOpen.delete(c); else state.treeOpen.add(c);
      renderSidebar();
    } else {
      state.treeOpen.add(c);
      applyCategory(c);
    }
    return;
  }
  if (act === 'rule') {
    const id = Number(btn.dataset.id);
    const r = (state.snap.rules || []).find(function (x) { return x.id === id; });
    if (!r) return;
    state.focusId = id;
    if (state.filters.category !== r.category) {
      state.treeOpen.add(r.category);
      applyCategory(r.category);
    } else {
      renderSidebar();
      renderRows();
    }

    const tr = document.querySelector('#rows tr[data-id="' + id + '"]');
    if (tr) tr.scrollIntoView({ block: 'center' });
  }
}

function syncSidebarToggle() {
  const sb = $('sidebar');
  const btn = $('btn-sidebar');
  if (!sb || !btn) return;
  const collapsed = sb.classList.contains('collapsed');
  btn.textContent = collapsed ? '»' : '«';
  btn.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
}

/** 侧栏收起 / 展开：窄窗口下让出横向空间，状态记在 localStorage。 */
function toggleSidebar() {
  const sb = $('sidebar');
  if (!sb) return;
  sb.classList.toggle('collapsed');
  try { localStorage.setItem(SIDEBAR_KEY, sb.classList.contains('collapsed') ? '1' : '0'); } catch (e) { /* 忽略 */ }
  syncSidebarToggle();
}

(function initSidebar() {
  const sb = $('sidebar');
  if (!sb) return;
  // 没有历史偏好时，窄窗口默认收起成窄栏，把横向空间让给表格。
  let collapsed = null;
  try {
    const stored = localStorage.getItem(SIDEBAR_KEY);
    if (stored !== null) collapsed = stored === '1';
  } catch (e) { /* 忽略 */ }
  if (collapsed === null) collapsed = window.innerWidth < 820;
  if (collapsed) sb.classList.add('collapsed');
  syncSidebarToggle();
  sb.addEventListener('click', function (ev) {
    if (ev.target.closest('#btn-sidebar')) { toggleSidebar(); return; }
    onTreeClick(ev);
  });
})();

/** 分类 / 状态 / 搜索三项筛选（**不含隐藏态**）。
 *  单独抽出来是为了把"被隐藏的"和"被真筛掉的"分开统计：列表空的时候，
 *  界面要能说清是哪一种（两种情况的提示语不一样）。 */
function matchesFilters(r) {
  const f = state.filters;
  if (f.category && r.category !== f.category) return false;
  if (f.onlyEnabled && !r.want_enabled) return false;
  if (f.onlyAttention && !r.needs_attention) return false;
  const q = f.q.trim().toLowerCase();
  if (q) {
    const hay = (r.name + ' ' + r.source + ' ' + r.source_template + ' ' + r.target + ' ' + r.target_subpath).toLowerCase();
    if (hay.indexOf(q) < 0) return false;
  }
  return true;
}

/** 通过其它筛选、只是被隐藏掉的条目数（用于筛选摘要与空态提示）。 */
function hiddenMatchCount() {
  return (state.snap.rules || []).filter(function (r) {
    return r.hidden && matchesFilters(r);
  }).length;
}

function filteredRules() {
  const rows = (state.snap.rules || []).filter(function (r) {
    if (!matchesFilters(r)) return false;
    if (r.hidden && !state.filters.showHidden) return false;
    return true;
  });
  return sortRows(rows);
}

// ---------------------------------------------------------------------------
// 内置模板（还没添加成规则的预设）
//
// 预设库（内置 + 用户外置）本来的唯一入口是「添加规则」弹窗顶部的下拉。
// 这里把"还没建过规则"的模板直接铺成主列表的行（带「未添加」标记）：
// 勾选后点「应用选中」就自动建规则 + 建链，不用再开表单弹窗逐字段填。
// ---------------------------------------------------------------------------

/** 模板身份键：source_template 是路径模板，Windows 路径大小写不敏感。 */
function tplKey(tpl) { return String(tpl || '').toLowerCase(); }

/** 主列表里要显示的模板行：排除已经建过同源规则的，再套用当前的分类 / 搜索筛选。
 *  「只看已开启 / 只看需处理」描述的是规则状态，模板还没有状态 —— 这两项筛选
 *  生效时直接不展示模板，免得用户以为它们是规则。 */
function visibleTemplates() {
  const f = state.filters;
  if (f.onlyEnabled || f.onlyAttention) return [];
  const used = new Set((state.snap.rules || []).map(function (r) { return tplKey(r.source_template); }));
  const q = f.q.trim().toLowerCase();
  return (state.presets || []).filter(function (p) {
    if (used.has(tplKey(p.source_template))) return false;
    if (f.category && p.category !== f.category) return false;
    if (q) {
      const hay = (p.name + ' ' + p.source + ' ' + p.source_template + ' ' + p.target_subpath).toLowerCase();
      if (hay.indexOf(q) < 0) return false;
    }
    return true;
  });
}

/** 模板建规则时默认落到的目标根：与「添加规则」弹窗的默认值一致（第一个根）。 */
function tplTargetRoot() {
  const roots = (state.snap && state.snap.roots) || [];
  return roots[0] || null;
}

/** 模板行的目标路径预览（根 + 模板的默认子路径）。 */
function tplTargetPath(p) {
  const root = tplTargetRoot();
  if (!root) return p.target_subpath;
  return String(root.path).replace(/[\\\/]+$/, '') + '\\' + p.target_subpath;
}

/** 模板 → add_rule 的表单（字段名必须是 snake_case，契约见 view.rs 的 RuleForm）。 */
function templateForm(p, rootId) {
  return {
    name: p.name,
    category: p.category,
    source_template: p.source_template,
    target_root_id: rootId,
    target_subpath: p.target_subpath,
    link_type: 'junction',
    redirect_mode: p.redirect_mode || 'link',
    env_vars: p.env_vars || [],
    env_scope: p.env_scope || 'user',
    want_enabled: true,
    rebuild_on_boot: true,
    notes: p.note || null,
    excludes: [],
    auto_clean_policy: '',
  };
}

/** 勾选状态清理：模板从列表消失（后端不再返回 / 已建成规则）时同步取消勾选。
 *  只看模板自身是否还存在，不受当前筛选影响 —— 与规则勾选的保留语义一致。 */
function pruneTplPicks() {
  if (!state.pickedTpl.size) return;
  const used = new Set((state.snap.rules || []).map(function (r) { return tplKey(r.source_template); }));
  const alive = new Set(
    (state.presets || [])
      .filter(function (p) { return !used.has(tplKey(p.source_template)); })
      .map(function (p) { return tplKey(p.source_template); })
  );
  state.pickedTpl.forEach(function (k) { if (!alive.has(k)) state.pickedTpl.delete(k); });
}

/** 把当前勾选的模板逐个建成规则（add_rule），返回新建规则的 id 列表。
 *  建失败的单独弹 toast 并跳过，不拖累其余模板。 */
async function addPickedTemplates() {
  const picked = (state.presets || []).filter(function (p) { return state.pickedTpl.has(tplKey(p.source_template)); });
  if (!picked.length) return [];
  const root = tplTargetRoot();
  if (!root) {
    toast(t('rule.no_root_title'), t('rule.no_root_hint'), 'warn');
    return [];
  }
  const ids = [];
  for (const p of picked) {
    try {
      const id = await invoke('add_rule', { form: templateForm(p, root.id) });
      ids.push(id);
    } catch (e) {
      toast(tf('tpl.add_failed', { name: p.name }), e.message, 'error');
    }
  }
  return ids;
}

/** 表头点击排序：客户端、不写库（持久顺序靠拖拽 sort_order）。
 *  key=null 时原样返回（保持后端顺序）。badge 按"好→坏"排，旗标按
 *  期望开启优先、再按开机重建。 */
function sortRows(rows) {
  const s = state.sort;
  if (!s.key) return rows;
  const dir = s.dir === 'desc' ? -1 : 1;
  const badgeRank = { '✅': 0, '⚠️': 1, '❌': 2, '🚫': 3 };
  const val = function (r) {
    switch (s.key) {
      case 'badge': return badgeRank[r.badge] !== undefined ? badgeRank[r.badge] : 9;
      case 'name': return r.name || '';
      case 'category': return r.category || '';
      case 'source': return r.source || '';
      case 'target': return r.target || '';
      case 'flag': return (r.want_enabled ? 0 : 1) + ',' + (r.rebuild_on_boot ? 0 : 1);
      case 'usage': return (state.diskUsage && state.diskUsage[r.id]) ? state.diskUsage[r.id].bytes : -1;
      default: return '';
    }
  };
  return rows.slice().sort(function (a, b) {
    const va = val(a), vb = val(b);
    if (va < vb) return -dir;
    if (va > vb) return dir;
    return 0;
  });
}

/** 在当前排序列表头显示 ↑/↓，其余列清掉。表头是静态 HTML，每次 render 重画指示。 */
function renderSortIndicators() {
  document.querySelectorAll('th[data-sort]').forEach(function (th) {
    const old = th.querySelector('.sort-ind');
    if (old) old.remove();
    if (th.dataset.sort !== state.sort.key) return;
    const ind = document.createElement('span');
    ind.className = 'sort-ind';
    ind.textContent = state.sort.dir === 'desc' ? ' ↓' : ' ↑';
    th.appendChild(ind);
  });
}

/** 让筛选区与表头两处「全选」复选框与当前勾选状态保持一致。 */
function syncSelectAll(rows) {
  const total = rows.length;
  let picked = 0;
  rows.forEach(function (r) { if (state.picked.has(r.id)) picked++; });
  const checked = total > 0 && picked === total;
  const indeterminate = picked > 0 && picked < total;
  [$('sel-all'), $('sel-all-head')].forEach(function (cb) {
    if (!cb) return;
    cb.checked = checked;
    cb.indeterminate = indeterminate;
  });
}

function renderRows() {
  // 重画会销毁正在编辑的单元格：放弃旧编辑器的引用，避免之后误提交已脱离 DOM 的输入。
  activeCellEditor = null;
  // 保存 / 刷新后保持横向与纵向滚动位置，连续编辑时列表不跳回顶部。
  const wrap = document.querySelector('.table-wrap');
  const keepTop = wrap ? wrap.scrollTop : 0;
  const keepLeft = wrap ? wrap.scrollLeft : 0;
  // 重画行会丢掉 .ctx-target 高亮，菜单留着也没意义 —— 直接收起。
  if (ctxTarget) closeRowMenu();
  const rows = filteredRules();
  const tpls = visibleTemplates();
  const tbody = $('rows');
  tbody.textContent = '';

  // 「已隐藏 N」只在有隐藏条目时出现，且**不受当前筛选影响**（说的是"你藏了多少"，
  // 不是"这一屏藏了多少"），否则会被误读成筛选结果。
  const hiddenTotal = (state.snap.rules || []).filter(function (r) { return r.hidden; }).length;
  $('filter-summary').textContent =
    tf('filter.summary', { shown: rows.length, total: (state.snap.rules || []).length }) +
    (totalPicked() ? tf('filter.summary_picked', { n: totalPicked() }) : '') +
    (hiddenTotal ? tf('filter.summary_hidden', { n: hiddenTotal }) : '');
  syncSelectAll(rows);

  const empty = $('empty');
  if (!rows.length && !tpls.length && !state.draft) {
    empty.classList.remove('hidden');
    // 三选一：被隐藏了 / 被筛选了 / 一条规则都还没有。
    if (state.snap.rules.length && !state.filters.showHidden && hiddenMatchCount()) {
      empty.textContent = tf('empty.all_hidden', { n: hiddenMatchCount() });
    } else {
      empty.textContent = state.snap.rules.length
        ? t('empty.no_match')
        : t('empty.no_rules');
    }
    return;
  }
  empty.classList.add('hidden');

  const frag = document.createDocumentFragment();
  // 新增规则草稿固定插在列表最上面（筛选 / 排序都不影响它，方便连续填）。
  if (state.draft) frag.appendChild(renderDraftRow());
  rows.forEach(function (r) {
    const tr = document.createElement('tr');
    tr.dataset.id = String(r.id);
    if (state.picked.has(r.id)) tr.className += ' picked';
    if (r.id === state.focusId) tr.className += ' selected';
    // 已隐藏的条目只在「显示已隐藏」打开时才会被渲染；整行淡一点 + 名字后带标记，
    // 免得和正常条目混淆（它们照样参与开机重建与巡检，只是不占列表位置）。
    if (r.hidden) tr.className += ' row-hidden';

    /** 可编辑单元格：单击进入控件，回车 / 失焦保存（见 beginCellEdit）。 */
    const editTd = function (field, cls) {
      const td = document.createElement('td');
      td.className = 'cell-edit' + (cls ? ' ' + cls : '');
      td.dataset.id = String(r.id);
      td.dataset.field = field;
      td.title = t('table.edit_tip');
      // 保存后可以把焦点还给这一格（Enter / F2 再次进入编辑）。
      td.tabIndex = -1;
      tr.appendChild(td);
      return td;
    };

    // 勾选框 + 拖拽把手
    const tdCheck = document.createElement('td');
    const handle = document.createElement('span');
    handle.className = 'drag-handle';
    handle.textContent = '⋮⋮';
    handle.title = t('row.drag_tip');
    tdCheck.appendChild(handle);
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = state.picked.has(r.id);
    cb.dataset.act = 'pick';
    cb.dataset.id = String(r.id);
    tdCheck.appendChild(cb);
    tr.appendChild(tdCheck);

    // 状态徽标：只给图标，完整状态文字放 title（需要的细节在「名称」下方与悬停提示里）。
    const tdBadge = document.createElement('td');
    const badge = document.createElement('span');
    badge.className = 'badge ' + badgeClass(r.badge);
    badge.textContent = r.badge || '❔';
    badge.title = r.label;
    tdBadge.appendChild(badge);
    tr.appendChild(tdBadge);

    // 名称 + 需要处理的原因
    const tdName = editTd('name', 'cell-name-wrap');
    const nm = document.createElement('div');
    nm.className = 'cell-name';
    nm.textContent = r.name;
    if (r.hidden) {
      const tag = document.createElement('span');
      tag.className = 'hidden-tag';
      tag.textContent = t('row.hidden_tag');
      tag.title = t('row.hidden_tip');
      nm.appendChild(tag);
    }
    tdName.appendChild(nm);
    if (r.needs_attention) {
      const note = document.createElement('div');
      note.className = 'row-note err';
      note.textContent = r.label;
      tdName.appendChild(note);
    } else if (r.warnings && r.warnings.length) {
      const note = document.createElement('div');
      note.className = 'row-note';
      note.textContent = r.warnings[0];
      tdName.appendChild(note);
    }

    // 分类
    const tdCat = editTd('category');
    const pill = document.createElement('span');
    pill.className = 'pill';
    pill.textContent = categoryLabel(r.category);
    tdCat.appendChild(pill);

    // 重定向方式 + 链接类型
    //
    // 两种模式共用一列：
    // * link     —— 在源路径建 Junction / 符号链接（子项显示链接类型）；
    // * setting  —— 直接改注册表里的环境变量（子项显示变量名与作用域）。
    const tdLink = editTd('redirect_mode', 'cell-link');
    if (r.redirect_mode === 'setting') {
      const mode = document.createElement('span');
      mode.className = 'mode-setting';
      mode.textContent = t('badge.setting');
      tdLink.appendChild(mode);
      const detail = document.createElement('div');
      detail.className = 'cell-sub';
      detail.textContent = (r.env_vars || []).join('/') + ' · ' + t(
        r.env_scope === 'machine' ? 'sysenv.scope.machine' : 'sysenv.scope.user');
      tdLink.appendChild(detail);
    } else {
      tdLink.textContent = r.link_type === 'symlink_dir' ? t('badge.symlink') : t('badge.junction');
    }

    // 源路径
    const tdSrc = editTd('source', 'cell-source');
    const src = document.createElement('div');
    src.className = 'cell-path';
    src.textContent = r.source;
    src.title = r.source_template;
    tdSrc.appendChild(src);

    // 目标
    const tdTgt = editTd('target', 'cell-target');
    const tgt = document.createElement('div');
    tgt.className = 'cell-path root';
    tgt.textContent = r.target;
    tdTgt.appendChild(tgt);

    // 占用空间（MB）：只有点过「空间统计」才有数，未统计时给占位符。
    const tdUsage = document.createElement('td');
    tdUsage.className = 'cell-usage';
    const usageEntry = state.diskUsage ? state.diskUsage[r.id] : null;
    if (usageEntry) {
      tdUsage.textContent = (usageEntry.bytes / 1048576).toFixed(1);
      tdUsage.title = tf('row.usage_title', { size: usageEntry.bytes_text, files: usageEntry.files });
    } else {
      tdUsage.className += ' none';
      tdUsage.textContent = '—';
      tdUsage.title = t('row.usage_pending');
    }
    tr.appendChild(tdUsage);

    // 旗标：期望开启 + 开机重建合并成一列，单击即切换并保存。
    const tdFlags = document.createElement('td');
    tdFlags.className = 'cell-flags';
    const flags = document.createElement('div');
    flags.className = 'row-flags';
    const on = document.createElement('span');
    on.className = 'toggle clickable ' + (r.want_enabled ? 'on' : 'off');
    on.textContent = r.want_enabled ? '●' : '○';
    on.title = r.want_enabled ? t('row.expect_on') : t('row.expect_off');
    on.dataset.act = 'toggle-flag';
    on.dataset.id = String(r.id);
    on.dataset.flag = 'want_enabled';
    flags.appendChild(on);
    const bt = document.createElement('span');
    bt.className = 'toggle clickable ' + (r.rebuild_on_boot ? 'on' : 'off');
    bt.textContent = r.rebuild_on_boot ? '⟳' : '·';
    bt.title = r.rebuild_on_boot ? t('row.rebuild_on') : t('row.rebuild_off');
    bt.dataset.act = 'toggle-flag';
    bt.dataset.id = String(r.id);
    bt.dataset.flag = 'rebuild_on_boot';
    flags.appendChild(bt);
    tdFlags.appendChild(flags);
    tr.appendChild(tdFlags);

    // 自动清理策略
    const tdAuto = editTd('clean_policy', 'cell-auto');
    tdAuto.textContent = autoPolicyShort(r.auto_clean_policy);
    if (!r.auto_clean_policy) tdAuto.classList.add('none');

    // 文件夹容量上限
    const tdLimit = editTd('clean_limit', 'cell-limit');
    if (r.auto_clean_limit_bytes > 0) {
      tdLimit.textContent = humanBytes(r.auto_clean_limit_bytes);
    } else {
      tdLimit.textContent = '—';
      tdLimit.classList.add('none');
    }

    // 清理排除
    const tdEx = editTd('excludes', 'cell-excludes');
    if (r.excludes && r.excludes.length) {
      tdEx.textContent = tf('table.excludes_count', { n: r.excludes.length });
      tdEx.title = r.excludes.join(' · ') + '\n' + t('table.edit_tip');
    } else {
      tdEx.textContent = '—';
      tdEx.classList.add('none');
    }

    // 备注
    const tdNotes = editTd('notes', 'cell-notes');
    tdNotes.textContent = r.notes || '—';
    if (!r.notes) tdNotes.classList.add('none');

    // 行内操作（编辑已并入单元格，这里只保留执行动作）
    const tdAct = document.createElement('td');
    const wrap = document.createElement('div');
    wrap.className = 'row-actions';
    [
      { act: 'apply', label: t('row.act.apply'), primary: true, when: r.badge !== '✅' },
      { act: 'restore', label: t('row.act.restore'), when: r.badge === '✅' || r.badge === '⚠️' },
      { act: 'relink', label: t('row.act.relink'), when: r.badge === '⚠️' || r.badge === '❌' },
      { act: 'clean', label: t('row.act.clean'), when: true },
    ].forEach(function (b) {
      if (!b.when) return;
      const btn = document.createElement('button');
      btn.className = 'mini' + (b.primary ? ' primary' : '');
      btn.textContent = b.label;
      btn.dataset.act = b.act;
      btn.dataset.id = String(r.id);
      wrap.appendChild(btn);
    });
    tdAct.appendChild(wrap);
    tr.appendChild(tdAct);

    frag.appendChild(tr);
  });
  // 内置模板行排在真实规则之后：它们只是"可一键添加的预设"（.tpl-row 淡背景 +
  // 「未添加」徽标），与库里已有规则视觉上分开。
  tpls.forEach(function (p) { frag.appendChild(templateRow(p)); });
  tbody.appendChild(frag);
  if (wrap) {
    wrap.scrollTop = keepTop;
    wrap.scrollLeft = keepLeft;
  }
}

/** 单条内置模板行。勾选后随「应用选中」一并建规则 + 建链，全程不开表单。 */
function templateRow(p) {
  const key = tplKey(p.source_template);
  const tr = document.createElement('tr');
  tr.className = 'tpl-row';
  tr.dataset.tpl = p.source_template;

  // 勾选框（模板没有 id，用 source_template 当键）
  const tdCheck = document.createElement('td');
  const cb = document.createElement('input');
  cb.type = 'checkbox';
  cb.checked = state.pickedTpl.has(key);
  cb.dataset.act = 'pick-tpl';
  cb.dataset.tpl = p.source_template;
  tdCheck.appendChild(cb);
  tr.appendChild(tdCheck);

  // 状态徽标：只给图标（虚线蓝底表示"还没添加成规则"），文案放 title。
  const tdBadge = document.createElement('td');
  const badge = document.createElement('span');
  badge.className = 'badge tpl';
  badge.textContent = '＋';
  badge.title = t('tpl.badge_tip');
  tdBadge.appendChild(badge);
  tr.appendChild(tdBadge);

  // 名称 + 提示（有 note 用 note，没有就给默认的"勾选即应用"说明）
  const tdName = document.createElement('td');
  tdName.className = 'tpl-pick';
  tdName.dataset.act = 'tpl-draft';
  tdName.dataset.tpl = p.source_template;
  tdName.title = t('tpl.draft_tip');
  const nm = document.createElement('div');
  nm.className = 'cell-name';
  nm.textContent = p.name;
  tdName.appendChild(nm);
  const note = document.createElement('div');
  note.className = 'row-note';
  note.textContent = p.note ? ('⚠ ' + p.note) : t('tpl.hint');
  tdName.appendChild(note);
  tr.appendChild(tdName);

  // 分类
  const tdCat = document.createElement('td');
  const pill = document.createElement('span');
  pill.className = 'pill';
  pill.textContent = categoryLabel(p.category);
  tdCat.appendChild(pill);
  tr.appendChild(tdCat);

  // 重定向方式：模板按预设带的模式显示（默认 junction 链接）。
  const tdLink = document.createElement('td');
  tdLink.className = 'cell-link';
  if (p.redirect_mode === 'setting') {
    const mode = document.createElement('span');
    mode.className = 'mode-setting';
    mode.textContent = t('badge.setting');
    tdLink.appendChild(mode);
    const detail = document.createElement('div');
    detail.className = 'cell-sub';
    detail.textContent = (p.env_vars || []).join('/') + ' · ' + t(
      p.env_scope === 'machine' ? 'sysenv.scope.machine' : 'sysenv.scope.user');
    tdLink.appendChild(detail);
  } else {
    tdLink.textContent = t('badge.junction');
  }
  tr.appendChild(tdLink);

  // 源路径（展开后的路径，title 显示原始模板）
  const tdSrc = document.createElement('td');
  const src = document.createElement('div');
  src.className = 'cell-path';
  src.textContent = p.source;
  src.title = p.source_template;
  tdSrc.appendChild(src);
  tr.appendChild(tdSrc);

  // 目标（预计落在第一个目标根下）
  const tdTgt = document.createElement('td');
  const tgt = document.createElement('div');
  tgt.className = 'cell-path root';
  tgt.textContent = tplTargetPath(p);
  tdTgt.appendChild(tgt);
  tr.appendChild(tdTgt);

  // 占用空间：模板还没建规则，没有可统计的缓存目录。
  const tdUsage = document.createElement('td');
  tdUsage.className = 'cell-usage none';
  tdUsage.textContent = '—';
  tdUsage.title = t('row.usage_pending');
  tr.appendChild(tdUsage);

  // 旗标：模板还没有状态，按"新建即期望开启 + 开机重建"灰显
  const tdFlags = document.createElement('td');
  const flags = document.createElement('div');
  flags.className = 'row-flags';
  [['●', t('row.expect_on')], ['⟳', t('row.rebuild_on')]].forEach(function (f2) {
    const s = document.createElement('span');
    s.className = 'toggle tpl';
    s.textContent = f2[0];
    s.title = f2[1];
    flags.appendChild(s);
  });
  tdFlags.appendChild(flags);
  tr.appendChild(tdFlags);

  // 模板还没有清理策略 / 容量上限 / 排除（建规则后再在行内设置）。
  ['cell-auto', 'cell-limit', 'cell-excludes', 'cell-notes'].forEach(function (cls) {
    const td = document.createElement('td');
    td.className = cls + ' none';
    td.textContent = '—';
    tr.appendChild(td);
  });

  // 行操作：一键"添加并应用"（与勾选后按「应用选中」等价，只是省一步）
  const tdAct = document.createElement('td');
  const wrap = document.createElement('div');
  wrap.className = 'row-actions';
  const btn = document.createElement('button');
  btn.className = 'mini primary';
  btn.textContent = t('tpl.add_apply');
  btn.dataset.act = 'tpl-apply';
  btn.dataset.tpl = p.source_template;
  wrap.appendChild(btn);
  tdAct.appendChild(wrap);
  tr.appendChild(tdAct);

  return tr;
}


// ---------------------------------------------------------------------------
// 弹窗
// ---------------------------------------------------------------------------

/** 需要"等用户选一个"的弹窗（✕ / Esc 也会 resolve 成 cancel，避免 Promise 悬挂）。 */
let modalResolver = null;

function openModal(opts) {
  $('modal-title').textContent = opts.title;
  const body = $('modal-body');
  body.textContent = '';
  // 只接受 DOM 节点：**不用 innerHTML**。规则名/路径都来自用户数据，
  // 一旦哪天有人图省事拼字符串，innerHTML 就是一个注入点。
  if (opts.body) body.appendChild(opts.body);
  $('modal-card').className = 'modal-card' + (opts.wide ? ' wide' : '');

  const foot = $('modal-foot');
  foot.textContent = '';
  (opts.actions || []).forEach(function (a) {
    const b = document.createElement('button');
    b.className = 'btn ' + (a.kind || 'ghost');
    b.textContent = a.label;
    if (a.disabled) b.disabled = true;
    b.addEventListener('click', function () { a.onClick(); });
    foot.appendChild(b);
  });
  $('modal').classList.remove('hidden');
}

function closeModal() {
  $('modal').classList.add('hidden');
  $('modal-body').textContent = '';
  const r = modalResolver;
  modalResolver = null;
  if (r) r('cancel');
}

function askChoice(opts) {
  return new Promise(function (resolve) {
    let settled = false;
    function finish(value) {
      if (settled) return;
      settled = true;
      modalResolver = null;
      $('modal').classList.add('hidden');
      $('modal-body').textContent = '';
      resolve(value);
    }
    modalResolver = finish;

    const body = document.createElement('div');
    (opts.lines || []).forEach(function (line) {
      const p = document.createElement('div');
      p.className = line.kind ? 'hint ' + line.kind : 'field';
      p.textContent = line.text;
      body.appendChild(p);
    });
    openModal({
      title: opts.title,
      body: body,
      actions: (opts.actions || []).map(function (a) {
        return {
          label: a.label,
          kind: a.kind,
          onClick: function () { finish(a.value); },
        };
      }),
    });
  });
}

// ---------------------------------------------------------------------------
// 进度
// ---------------------------------------------------------------------------

function showProgress(title) {
  $('progress-title').textContent = title;
  $('progress-fill').style.width = '0%';
  $('progress-detail').textContent = '';
  $('progress').classList.remove('hidden');
}

function setProgress(pct, detail) {
  $('progress-fill').style.width = Math.max(0, Math.min(100, pct)) + '%';
  $('progress-detail').textContent = detail || '';
}

function hideProgress() { $('progress').classList.add('hidden'); }

// ---------------------------------------------------------------------------
// 动作
// ---------------------------------------------------------------------------

function pickedIds() { return Array.from(state.picked); }

/** 勾选总数：规则 + 还没添加成规则的内置模板。 */
function totalPicked() { return state.picked.size + state.pickedTpl.size; }

function requirePicked(what) {
  if (!state.picked.size) {
    toast(t('toast.no_pick'), tf('toast.no_pick_hint', { what: what }), 'warn');
    return false;
  }
  return true;
}

/** 「应用选中」专用：规则和内置模板都算数（模板会先建成规则再一起建链）。
 *  其余批量操作（恢复 / 开关 / 删除 / …）只对已有规则有意义，仍走 requirePicked。 */
function requireAnyPick(what) {
  if (!totalPicked()) {
    toast(t('toast.no_pick'), tf('toast.no_pick_hint', { what: what }), 'warn');
    return false;
  }
  return true;
}

/** 勾选的模板先建规则，再和已勾选的规则一起交回给调用方建链。 */
async function applyPickedAll() {
  const created = await addPickedTemplates();
  const ids = pickedIds().concat(created);
  if (!ids.length) return;
  await runApply(ids, false);
}

/** 批量开启 / 关闭规则（工具栏「开启 / 关闭」与行右键菜单共用）。 */
async function setRulesEnabled(ids, enabled) {
  if (!ids.length) return;
  try {
    await invoke('set_rules_enabled', { ids: ids, enabled: enabled });
    toast(enabled ? t('toast.enabled') : t('toast.disabled'), tf('toast.toggle_hint', { n: ids.length }), 'ok');
    await refresh(true);
  } catch (e) { toast(t('toast.op_failed'), e.message, 'error'); }
}

/** 隐藏 / 取消隐藏条目（行右键菜单）。
 *
 *  隐藏是**展示偏好**：后端把它存在 settings 里（见 usecase::rules::KEY_HIDDEN_RULES），
 *  规则本身一切照旧 —— 开机重建、水位巡检、清理都还管它，只是不占列表位置。
 *  取消隐藏后行会立刻回到列表（因为快照里 hidden 已经变回 false）。 */
async function setRulesHidden(ids, hidden) {
  if (!ids.length) return;
  try {
    await invoke('set_rules_hidden', { ids: ids, hidden: hidden });
    // 隐藏掉的条目要从勾选集合里摘掉：看不见的行不该再被「应用选中 / 删除」带上。
    if (hidden) ids.forEach(function (id) { state.picked.delete(id); });
    toast(
      t('toast.hide_done'),
      hidden ? tf('toast.hide_done_hint', { n: ids.length }) : tf('toast.unhide_done_hint', { n: ids.length }),
      'ok'
    );
    await refresh(true);
  } catch (e) { toast(t('toast.op_failed'), e.message, 'error'); }
}

/** 删除规则的确认弹窗（工具栏「删除」与行右键菜单共用）。 */
function confirmDeleteRules(ids) {
  if (!ids.length) return;
  const body = document.createElement('div');
  const p1 = document.createElement('div');
  p1.className = 'field';
  p1.textContent = tf('del.confirm_body', { n: ids.length });
  const p2 = document.createElement('div');
  p2.className = 'field';
  const h = document.createElement('div');
  h.className = 'hint warn';
  h.textContent = t('del.confirm_warn');
  p2.appendChild(h);
  body.appendChild(p1);
  body.appendChild(p2);
  openModal({
    title: t('del.confirm_title'),
    body: body,
    actions: [
      { label: t('common.cancel'), onClick: closeModal },
      {
        label: t('common.delete'), kind: 'danger',
        onClick: async function () {
          try {
            await invoke('delete_rules', { ids: ids });
            state.picked.clear();
            closeModal();
            toast(t('del.done'), tf('del.done_hint', { n: ids.length }), 'ok');
            await refresh(false);
          } catch (e) { toast(t('del.failed'), e.message, 'error'); }
        },
      },
    ],
  });
}

/**
 * 撤销最近一次操作：删除（把删掉的规则记录搬回规则库）或修改（把字段改回去）。
 *
 * 撤销只动记录、不碰磁盘上的目录与链接，所以不需要提权、不搬数据。
 * 删掉的那一批一起恢复（不是只恢复一条）；被改过的字段按"改前值"逐个改回去。
 */
async function undoLast() {
  try {
    const out = await invoke('undo_last', {});
    const restored = (out && out.restored) || [];
    const skipped = (out && out.skipped) || [];
    const kind = (out && out.kind) || '';
    if (!restored.length && !skipped.length) {
      toast(t('undo.done_none'), t('undo.done_none_hint'), 'warn');
    } else if (skipped.length) {
      toast(t('undo.partial'), tf('undo.partial_hint', { n: restored.length, m: skipped.length }) +
        (skipped.length ? '：' + skipped.join('；') : ''), 'warn');
    } else if (kind === 'update') {
      toast(t('undo.done_update'), tf('undo.done_update_hint', { n: restored.length }), 'ok');
    } else {
      toast(t('undo.done'), tf('undo.done_hint', { n: restored.length }), 'ok');
    }
    await refresh(true);
  } catch (e) {
    toast(t('undo.failed'), e.message, 'error');
  }
}

/** 「清理…」弹窗的公共入口：把规则 id 列表转成 targets 再复用 openCleanDialog。 */
async function openCleanTargets(ids) {
  const rules = ids.map(function (id) {
    return state.snap.rules.find(function (r) { return r.id === id; });
  }).filter(Boolean);
  if (!rules.length) {
    toast(t('clean.nothing'), t('clean.nothing_hint'), 'warn');
    return;
  }
  const targets = rules.map(function (r) { return { kind: 'rule', id: r.id }; });
  await openCleanDialog(
    targets,
    tf('clean.selected_batch_heading', { n: rules.length }),
    t('clean.selected_batch_title'),
  );
}

/** 勾选后批量归类：把选中的规则统一移到某个分类（复用已有分类或直接新建）。 */
function openCategorizeDialog() {
  const ids = pickedIds();
  if (!ids.length) return;

  const picked = ids.map(ruleById).filter(Boolean);
  // 选中项分类完全一致时预选它，否则从第一个分类开始选。
  const current = picked.length && picked.every(function (r) { return r.category === picked[0].category; })
    ? picked[0].category : '';

  const body = document.createElement('div');
  const tip = document.createElement('div');
  tip.className = 'field';
  tip.textContent = tf('categorize.body', { n: ids.length });
  body.appendChild(tip);

  const cats = state.snap.categories || [];
  const sel = document.createElement('select');
  sel.className = 'select';
  cats.forEach(function (c) {
    const o = document.createElement('option');
    o.value = c;
    o.textContent = categoryLabel(c);
    sel.appendChild(o);
  });
  // 空值 = 改走右侧输入框新建分类，和 Gmail「移动到 → 新建标签」同一种走法。
  const custom = document.createElement('option');
  custom.value = '';
  custom.textContent = t('categorize.new_option');
  sel.appendChild(custom);
  sel.value = current || cats[0] || '';
  body.appendChild(sel);

  const inp = document.createElement('input');
  inp.className = 'input';
  inp.type = 'text';
  inp.maxLength = 24;
  inp.placeholder = t('categorize.new_placeholder');
  body.appendChild(inp);

  function sync() {
    const isNew = sel.value === '';
    inp.classList.toggle('hidden', !isNew);
    if (isNew) inp.focus();
  }
  sel.addEventListener('change', sync);

  async function commit() {
    const category = (sel.value || inp.value).trim();
    if (!category) { toast(t('categorize.empty'), '', 'warn'); inp.focus(); return; }
    try {
      await invoke('set_rules_category', { ids: ids, category: category });
      // 归类后这些行可能已离开当前视图（正按分类筛选），清空勾选避免误操作。
      state.picked.clear();
      closeModal();
      toast(t('toast.categorized'),
        tf('toast.categorized_hint', { n: ids.length, cat: categoryLabel(category) }), 'ok');
      await refresh(true);
    } catch (e) { toast(t('toast.op_failed'), e.message, 'error'); }
  }
  // 新建分类时按回车直接提交（与弹窗里的「归类」按钮等价）。
  inp.addEventListener('keydown', function (ev) {
    if (ev.key === 'Enter') { ev.preventDefault(); commit(); }
  });

  openModal({
    title: t('categorize.title'),
    body: body,
    actions: [
      { label: t('common.cancel'), onClick: closeModal },
      { label: t('categorize.confirm'), kind: 'primary', onClick: commit },
    ],
  });
  sync();
  sel.focus();
}

/** 把一组内置模板逐个建成规则并立刻建链（模板行「添加并应用」与右键菜单共用）。 */
async function applyTemplates(tpls) {
  if (!tpls.length) return;
  const root = tplTargetRoot();
  if (!root) { toast(t('rule.no_root_title'), t('rule.no_root_hint'), 'warn'); return; }
  const ids = [];
  for (const p of tpls) {
    try {
      const newId = await invoke('add_rule', { form: templateForm(p, root.id) });
      ids.push(newId);
      state.pickedTpl.delete(tplKey(p.source_template));
    } catch (e) {
      toast(tf('tpl.add_failed', { name: p.name }), e.message, 'error');
    }
  }
  if (ids.length) await runApply(ids, false);
}

/** 用户是否打开了「应用时合并到已有目标目录」。
 *
 *  默认关闭：合并会把源目录并进已经有内容的目标目录，同名文件取修改时间
 *  较新者 —— 也就是目标里的旧文件可能被源的新版本覆盖。
 *  这个开关此前只存在于 CLI（`apply --merge`），界面上恒为 false，
 *  于是"目标目录已有内容"在界面上是一道过不去的坎。 */
function mergeEnabled() {
  return !!(state.uiSettings && state.uiSettings.allow_merge);
}

/** 应用用的选项（普通路径）。 */
function applyOpts(dryRun) {
  return {
    dry_run: !!dryRun,
    allow_merge: mergeEnabled(),
    skip_lock_check: false,
    best_effort: false,
    sha256: false,
  };
}

/** 提权用的选项（尽力模式）。合并开关照样带上：
 *  尽力模式本身已经放行"目标非空"，但把用户意图原样传下去，
 *  将来若调整尽力模式的语义，这里不用跟着改。 */
function elevatedOpts() {
  return {
    dry_run: false,
    allow_merge: mergeEnabled(),
    skip_lock_check: false,
    best_effort: true,
    sha256: false,
  };
}

/** 这批结果里有没有「系统设置模式」的规则真正生效（变量被改写了）。
 *
 *  判据用后端返回的 action，而不是前端的 redirect_mode：
 *  action 是引擎跑完之后的真实结果（`setting_applied` = 变量确实被指向目标），
 *  而 redirect_mode 只说明规则「配置成」这个模式 —— 预演、幂等返回、
 *  变量本来就指对了，都不会真的改注册表，这时提示"要重启"是误导。 */
function settingApplied(out) {
  return out.some(function (o) { return o.ok && o.action === 'setting_applied'; });
}

/** 「系统设置模式」生效后必须提醒：已运行的软件仍用着旧环境块。 */
function toastRestartHint(out) {
  if (!settingApplied(out)) return;
  toast(t('sysenv.restart_hint'), t('sysenv.restart_hint_detail'), 'warn');
}

/** 走提权 worker（会弹一次 UAC）。 */
async function runElevated(kind, ids) {
  if (!ids.length) return;
  state.busy = true;
  showProgress(t('act.elev_request'));
  try {
    const out = await invoke('run_elevated_job', { kind: kind, ids: ids, opts: elevatedOpts() });
    hideProgress();
    const ok = out.filter(function (o) { return o.ok; }).length;
    const failed = out.length - ok;
    out.forEach(function (o) {
      if (!o.ok) toast(tf('act.fail_item', { id: o.rule_id, name: o.name }), o.message, 'error');
    });
    toast(
      kind === 'apply' ? t('act.elev_apply_done') : t('act.elev_restore_done'),
      tf('act.summary', { ok: ok }) + (failed ? tf('act.summary_failed', { n: failed }) : ''),
      failed ? 'warn' : 'ok'
    );
    if (kind === 'apply') toastRestartHint(out);
    await refresh(true);
  } catch (e) {
    hideProgress();
    toast(t('act.elev_failed'), e.message, 'error');
  } finally {
    state.busy = false;
  }
}

/** 选中项里有没有"需要管理员权限"的规则。 */
function rulesNeedingElevation(ids) {
  return ids.filter(function (id) {
    const r = state.snap.rules.find(function (x) { return x.id === id; });
    return r && r.needs_elevation;
  });
}

async function runApply(ids, dryRun) {
  if (!ids.length) return;

  if (!dryRun) {
    const needElev = rulesNeedingElevation(ids);
    if (needElev.length) {
      const others = ids.length - needElev.length;
      const choice = await askChoice({
        title: t('act.elev_needed_title'),
        lines: [
          { text: tf('act.elev_needed_line1', { total: ids.length, n: needElev.length }) },
          {
            text: t('act.elev_needed_line2') +
              (others ? tf('act.elev_needed_line2_rest', { n: others }) : ''),
            kind: '',
          },
          { text: t('act.elev_needed_line3'), kind: '' },
        ],
        actions: [
          { label: t('act.elev_run'), kind: 'primary', value: 'elevate' },
          { label: t('act.elev_partial'), value: 'partial' },
          { label: t('common.cancel'), value: 'cancel' },
        ],
      });
      if (choice !== 'elevate' && choice !== 'partial') return;
      if (choice === 'elevate') return runElevated('apply', ids);
      if (choice === 'partial') {
        const rest = ids.filter(function (id) { return needElev.indexOf(id) < 0; });
        if (!rest.length) {
          toast(t('act.no_handle'), t('act.all_need_elev'), 'warn');
          return;
        }
        ids = rest;
      }
    }
  }

  state.busy = true;
  showProgress((dryRun ? t('act.dry_prefix') : '') + tf('act.processing', { n: ids.length }));
  try {
    const out = await invoke('apply_rules', { ids: ids, opts: applyOpts(dryRun) });
    const ok = out.filter(function (o) { return o.ok; }).length;
    const failed = out.length - ok;
    hideProgress();
    out.forEach(function (o) {
      if (!o.ok) toast(tf('act.fail_item', { id: o.rule_id, name: o.name }), o.message, 'error');
    });
    toast(dryRun ? t('act.dry_done') : t('act.apply_done'), tf('act.summary', { ok: ok }) + (failed ? tf('act.summary_failed', { n: failed }) : ''), failed ? 'warn' : 'ok');
    if (!dryRun) toastRestartHint(out);
    await refresh(true);
  } catch (e) {
    hideProgress();
    toast(t('act.apply_failed'), e.message, 'error');
  } finally {
    state.busy = false;
  }
}

async function runRestore(ids) {
  if (!ids.length) return;
  state.busy = true;
  showProgress(tf('act.restoring', { n: ids.length }));
  try {
    const out = await invoke('restore_rules', { ids: ids, opts: { dry_run: false, allow_merge: true, skip_lock_check: true, sha256: false } });
    const ok = out.filter(function (o) { return o.ok; }).length;
    const failed = out.length - ok;
    hideProgress();
    out.forEach(function (o) {
      if (!o.ok) toast(tf('act.fail_item', { id: o.rule_id, name: o.name }), o.message, 'error');
    });
    toast(t('act.restore_done'), tf('act.summary', { ok: ok }) + (failed ? tf('act.summary_failed', { n: failed }) : ''), failed ? 'warn' : 'ok');
    await refresh(true);
  } catch (e) {
    hideProgress();
    toast(t('act.restore_failed'), e.message, 'error');
  } finally {
    state.busy = false;
  }
}

/** 「恢复全部」panic 按钮：把所有**处于链接状态**的规则一键还原。
 *  场景：卸载软件 / 卸载 RAM 盘 / 出问题排查 —— 需要快回到"没有重定向"的世界。
 *  只认 ✅/⚠️（链接确实存在）的规则；plain_dir 本来就是真实目录，没什么可恢复。
 *  有提权规则时整批走提权进程（panic 场景少一次选择）。 */
async function restoreAllRules() {
  const linked = state.snap.rules.filter(function (r) {
    return r.badge === '✅' || r.badge === '⚠️';
  });
  if (!linked.length) {
    toast(t('act.all.nothing'), t('act.all.nothing_hint'), 'warn');
    return;
  }
  const needElev = linked.filter(function (r) { return r.needs_elevation; }).length;
  const ids = linked.map(function (r) { return r.id; });

  const body = document.createElement('div');
  const p1 = document.createElement('div');
  p1.className = 'field';
  p1.textContent = tf('act.all.intro', { n: linked.length });
  body.appendChild(p1);
  const p2 = document.createElement('div');
  p2.className = 'field';
  const h1 = document.createElement('div');
  h1.className = 'hint warn';
  h1.textContent = t('act.all.warn');
  p2.appendChild(h1);
  const h2 = document.createElement('div');
  h2.className = 'hint';
  h2.textContent = needElev
    ? tf('act.all.elev', { n: needElev })
    : t('act.all.keep_calm');
  p2.appendChild(h2);
  body.appendChild(p2);
  const list = document.createElement('div');
  list.className = 'field';
  list.textContent = tf('act.all.involved', { names: linked.map(function (r) { return r.name; }).join(t('list.sep')) });
  body.appendChild(list);

  openModal({
    title: t('act.all.title'),
    body: body,
    actions: [
      { label: t('common.cancel'), onClick: closeModal },
      {
        label: t('act.all.confirm'), kind: 'danger',
        onClick: async function () {
          closeModal();
          if (needElev) return runElevated('restore', ids);
          return runRestore(ids);
        },
      },
    ],
  });
}

async function doRelink(id) {
  try {
    const o = await invoke('relink_rule', { id: id });
    toast(o.ok ? t('act.relink_ok') : t('act.relink_fail'), o.message, o.ok ? 'ok' : 'error');
    await refresh(true);
  } catch (e) {
    toast(t('act.relink_failed'), e.message, 'error');
  }
}

async function doLocks(id) {
  toast(t('act.locks.scanning'), t('act.locks.scanning_hint'), '');
  try {
    const rep = await invoke('check_locks', { id: id });
    const body = document.createElement('div');
    if (rep.locked_files.length === 0 && rep.denied_files.length === 0) {
      const p = document.createElement('div');
      p.className = 'field';
      const hint = document.createElement('div');
      hint.className = 'hint ok';
      hint.textContent = tf('act.locks.clear', { n: rep.scanned, ms: rep.elapsed_ms });
      p.appendChild(hint);
      body.appendChild(p);
    } else {
      const p = document.createElement('div');
      p.className = 'field';
      const hint = document.createElement('div');
      hint.className = 'hint warn';
      hint.textContent = tf('act.locks.busy', { n: rep.locked_files.length }) + (rep.truncated ? t('act.locks.truncated') : '') +
        (rep.denied_files.length ? tf('act.locks.denied', { n: rep.denied_files.length }) : '');
      p.appendChild(hint);
      body.appendChild(p);

      if (rep.blockers.length) {
        const bl = document.createElement('div');
        bl.className = 'field';
        const lbl = document.createElement('label');
        lbl.textContent = t('act.locks.blockers');
        bl.appendChild(lbl);
        rep.blockers.forEach(function (b) {
          const line = document.createElement('div');
          line.className = 'hint err';
          line.textContent = '⛔ ' + b.app + '（PID ' + b.pid + '）' + (b.is_service ? t('act.locks.service') : '');
          bl.appendChild(line);
        });
        body.appendChild(bl);
      }
      const files = document.createElement('div');
      files.className = 'field';
      const fl = document.createElement('label');
      fl.textContent = t('act.locks.files');
      files.appendChild(fl);
      rep.locked_files.slice(0, 10).forEach(function (f) {
        const line = document.createElement('div');
        line.className = 'detail-val';
        line.textContent = f;
        files.appendChild(line);
      });
      body.appendChild(files);
    }
    openModal({ title: t('act.locks.title'), body: body, actions: [{ label: t('common.close'), onClick: closeModal }] });
    await refresh(true);
  } catch (e) {
    toast(t('act.locks.failed'), e.message, 'error');
  }
}

async function doBoot() {
  state.busy = true;
  showProgress(t('act.boot.running'));
  try {
    const r = await invoke('boot_rebuild');
    hideProgress();
    const lines = [r.roots.map(function (x) {
      return (x.ready ? '✅ ' : '❌ ') + x.path + (x.ready ? tf('act.boot.waited', { ms: x.waited_ms }) : '：' + (x.error || t('act.boot.not_ready')));
    }).join('\n'), r.one_line()].join('\n\n');
    toast(t('act.boot.done'), r.one_line(), r.has_problem() ? 'warn' : 'ok');
    if (r.warnings.length || r.failed.length) {
      const body = document.createElement('div');
      r.warnings.concat(r.failed.map(function (f) { return '#' + f[0] + ' ' + f[1]; })).forEach(function (w) {
        const d = document.createElement('div');
        d.className = 'hint warn';
        d.textContent = '⚠ ' + w;
        body.appendChild(d);
      });
      openModal({ title: t('act.boot.attention'), body: body, actions: [{ label: t('act.boot.got_it'), kind: 'primary', onClick: closeModal }] });
    }
    void lines;
    await refresh(true);
  } catch (e) {
    hideProgress();
    toast(t('act.boot.failed'), e.message, 'error');
  } finally {
    state.busy = false;
  }
}

async function openOps() {
  const body = document.createElement('div');
  // 效益统计条：三个数字与下面的操作流水同源（operations 表），
  // 放在运维弹窗里而不是首页 —— 它是"回顾"信息，不是"当前要处理"的信息。
  const strip = document.createElement('div');
  strip.className = 'savings-strip';
  strip.textContent = t('ops.savings_loading');
  body.appendChild(strip);
  loadSavings(strip);
  const tabs = document.createElement('div');
  tabs.className = 'log-tabs';
  body.appendChild(tabs);
  const list = document.createElement('div');
  list.className = 'log-list';
  body.appendChild(list);

  let mode = 'events';
  const btnEvents = document.createElement('button');
  btnEvents.className = 'btn active';
  btnEvents.textContent = t('ops.tab_events');
  const btnOps = document.createElement('button');
  btnOps.className = 'btn';
  btnOps.textContent = t('ops.tab_ops');
  tabs.appendChild(btnEvents);
  tabs.appendChild(btnOps);

  async function load() {
    btnEvents.className = 'btn' + (mode === 'events' ? ' active' : '');
    btnOps.className = 'btn' + (mode === 'ops' ? ' active' : '');
    list.textContent = '';
    try {
      if (mode === 'events') {
        const evs = await invoke('recent_events', { limit: 100 });
        if (!evs.length) { list.textContent = t('ops.no_events'); return; }
        evs.forEach(function (e) {
          list.appendChild(logRow(e.at, e.kind, e.message, e.level));
        });
      } else {
        const ops = await invoke('recent_ops', { limit: 100 });
        if (!ops.length) { list.textContent = t('ops.no_ops'); return; }
        ops.forEach(function (o) {
          const msg = '#' + o.id + ' rule=' + (o.rule_id === null ? '-' : o.rule_id) +
            ' files=' + (o.file_count || 0) + ' ' + (o.source_path || '') +
            (o.error ? '  ERR: ' + o.error : '');
          list.appendChild(logRow(o.started_at, o.kind + '/' + o.phase, msg, o.phase === 'done' ? 'info' : 'warn'));
        });
      }
    } catch (e) {
      list.textContent = tf('ops.load_failed', { msg: e.message });
    }
  }
  btnEvents.addEventListener('click', function () { mode = 'events'; load(); });
  btnOps.addEventListener('click', function () { mode = 'ops'; load(); });
  await load();
  openModal({ title: t('ops.title'), body: body, wide: true, actions: [{ label: t('common.close'), onClick: closeModal }] });
}

/** 效益统计三卡：当前重定向 / 累计搬运 / 累计清理。
 * 只报可解释的数字，不做"节省 SSD 寿命"外推 —— 后端同此约定（usecase/stats.rs）。 */
async function loadSavings(strip) {
  try {
    const v = await invoke('savings');
    strip.textContent = '';
    strip.appendChild(savingCard(t('ops.card.redirected_title'), v.redirected_text,
      tf('ops.card.redirected_sub', { rules: v.redirected_rules, files: v.redirected_files })));
    strip.appendChild(savingCard(t('ops.card.moved_title'), v.moved_text,
      tf('ops.card.moved_sub', { n: v.moved_files })));
    strip.appendChild(savingCard(t('ops.card.cleaned_title'), v.cleaned_text,
      tf('ops.card.cleaned_sub', { n: v.cleaned_files })));
  } catch (e) {
    // 统计只是附属信息，拿不到就撤掉这块，别挡住下面的日志。
    strip.remove();
  }
}

function savingCard(title, value, sub) {
  const card = document.createElement('div');
  card.className = 'saving-card';
  const titleEl = document.createElement('div');
  titleEl.className = 'saving-title';
  titleEl.textContent = title;
  const v = document.createElement('div');
  v.className = 'saving-value';
  v.textContent = value;
  const s = document.createElement('div');
  s.className = 'saving-sub';
  s.textContent = sub;
  card.appendChild(titleEl);
  card.appendChild(v);
  card.appendChild(s);
  return card;
}

function logRow(time, kind, msg, level) {
  const row = document.createElement('div');
  row.className = 'log-item ' + (level === 'error' ? 'error' : level === 'warn' ? 'warn' : '');
  const timeEl = document.createElement('div');
  timeEl.className = 'log-time';
  timeEl.textContent = time;
  const k = document.createElement('div');
  k.className = 'log-kind';
  k.textContent = kind;
  const m = document.createElement('div');
  m.className = 'log-msg';
  m.textContent = msg;
  row.appendChild(timeEl);
  row.appendChild(k);
  row.appendChild(m);
  return row;
}

async function openSettings() {
  const snap = state.snap;
  const body = document.createElement('div');

  const info = document.createElement('div');
  info.className = 'field';
  [
    [t('settings.version'), snap.version + tf('settings.schema_tag', { v: snap.schema_version })],
    [t('settings.data_dir'), snap.data_dir],
    [t('settings.db'), snap.db_path],
  ].forEach(function (kv) {
    const d = document.createElement('div');
    d.className = 'hint';
    d.textContent = tf('settings.kv', { k: kv[0], v: kv[1] });
    info.appendChild(d);
  });
  body.appendChild(info);

  // ---- 开机自启（M3）----
  await appendAutostartSection(body, snap);

  // ---- 关闭窗口行为 ----
  // 托盘常驻是水位巡检 / 预授权自动清理"无人值守"的前提：窗口关掉后前端
  // 定时器不再可靠，只有进程活着 + 托盘在，后台能力才继续生效。
  const closeField = document.createElement('div');
  closeField.className = 'field';
  const closeLabel = document.createElement('label');
  closeLabel.textContent = t('settings.close_when');
  closeField.appendChild(closeLabel);
  let closeToTray = true;
  let allowMerge = false;
  try {
    const prefs = await invoke('ui_settings', {});
    closeToTray = prefs.close_to_tray !== false;
    allowMerge = !!prefs.allow_merge;
  } catch (e) { closeToTray = true; allowMerge = false; }
  const closeCheck = document.createElement('input');
  closeCheck.type = 'checkbox';
  closeCheck.checked = closeToTray;
  const closeInline = document.createElement('label');
  closeInline.className = 'inline-check';
  closeInline.appendChild(closeCheck);
  const closeSpan = document.createElement('span');
  closeSpan.textContent = t('settings.close_to_tray');
  closeInline.appendChild(closeSpan);
  closeCheck.addEventListener('change', async function () {
    try {
      await invoke('set_close_to_tray', { enabled: closeCheck.checked });
      toast(t('common.saved'), closeCheck.checked ? t('settings.close_to_tray_on') : t('settings.close_to_tray_off'), 'ok');
    } catch (e) {
      toast(t('common.save_failed'), e.message, 'error');
    }
  });
  closeField.appendChild(closeInline);
  const closeHint = document.createElement('div');
  closeHint.className = 'hint';
  closeHint.textContent = t('settings.close_hint');
  closeField.appendChild(closeHint);
  body.appendChild(closeField);

  // ---- 应用时合并到已有目标目录 ----
  // 目标目录已经有内容时，普通应用会直接中止（"目标目录已有内容"）——
  // 这个开关把 CLI 的 `apply --merge` 能力搬到界面上。默认**关闭**：
  // 合并会覆盖目标里的同名旧文件（取修改时间较新者），属于用户该主动打开的。
  const mergeField = document.createElement('div');
  mergeField.className = 'field';
  const mergeLabel = document.createElement('label');
  mergeLabel.textContent = t('settings.merge_when');
  mergeField.appendChild(mergeLabel);
  const mergeInline = document.createElement('label');
  mergeInline.className = 'inline-check';
  const mergeCheck = document.createElement('input');
  mergeCheck.type = 'checkbox';
  mergeCheck.checked = allowMerge;
  mergeInline.appendChild(mergeCheck);
  const mergeSpan = document.createElement('span');
  mergeSpan.textContent = t('settings.merge_on');
  mergeInline.appendChild(mergeSpan);
  mergeCheck.addEventListener('change', async function () {
    try {
      await invoke('set_allow_merge', { enabled: mergeCheck.checked });
      state.uiSettings = Object.assign({}, state.uiSettings, { allow_merge: mergeCheck.checked });
      toast(t('common.saved'), mergeCheck.checked ? t('settings.merge_saved_on') : t('settings.merge_saved_off'), 'ok');
    } catch (e) {
      toast(t('common.save_failed'), e.message, 'error');
    }
  });
  mergeField.appendChild(mergeInline);
  const mergeHint = document.createElement('div');
  mergeHint.className = 'hint';
  mergeHint.textContent = t('settings.merge_hint');
  mergeField.appendChild(mergeHint);
  body.appendChild(mergeField);

  // ---- 定时（低峰期）自动清理 ----
  // 例行维护：每天到点把所有配了预授权策略（auto_clean_policy）的规则清一遍，
  // 让缓存别攒到触发水位。与水位自动清理共用同一套规则级授权，默认关闭。
  const schedField = document.createElement('div');
  schedField.className = 'field';
  const schedLabel = document.createElement('label');
  schedLabel.textContent = t('settings.sched');
  schedField.appendChild(schedLabel);
  let sched = { enabled: false, time: '03:00' };
  try { sched = await invoke('scheduled_clean_settings', {}); } catch (e) { /* 默认值 */ }
  const schedInline = document.createElement('label');
  schedInline.className = 'inline-check';
  const schedCheck = document.createElement('input');
  schedCheck.type = 'checkbox';
  schedCheck.checked = sched.enabled;
  schedInline.appendChild(schedCheck);
  const schedSpan = document.createElement('span');
  schedSpan.textContent = t('settings.sched_on');
  schedInline.appendChild(schedSpan);
  const schedTime = document.createElement('input');
  schedTime.type = 'time';
  schedTime.value = sched.time || '03:00';
  schedInline.appendChild(schedTime);
  async function saveSched() {
    try {
      await invoke('set_scheduled_clean', { enabled: schedCheck.checked, time: schedTime.value || '03:00' });
      toast(t('common.saved'), schedCheck.checked
        ? tf('settings.sched_saved_on', { time: schedTime.value || '03:00' })
        : t('settings.sched_saved_off'), 'ok');
    } catch (e) {
      toast(t('common.save_failed'), e.message, 'error');
    }
  }
  schedCheck.addEventListener('change', saveSched);
  schedTime.addEventListener('change', saveSched);
  schedField.appendChild(schedInline);
  const schedHint = document.createElement('div');
  schedHint.className = 'hint';
  schedHint.textContent = t('settings.sched_hint');
  schedField.appendChild(schedHint);
  body.appendChild(schedField);

  const rootField = document.createElement('div');
  rootField.className = 'field';
  const rootLabel = document.createElement('label');
  rootLabel.textContent = t('settings.root');
  rootField.appendChild(rootLabel);
  const rootList = document.createElement('div');
  rootList.className = 'log-list';
  (snap.roots || []).forEach(function (r) {
    const row = document.createElement('div');
    row.className = 'log-item';
    const a = document.createElement('div');
    a.className = 'log-kind';
    a.textContent = '#' + r.id + ' ' + r.label;
    const b = document.createElement('div');
    b.className = 'log-msg';
    b.textContent = r.path + ' · ' + (r.state === 'ready' ? t('header.root_ready') : r.message) +
      tf('settings.root_wait', { ms: r.wait_timeout_ms }) + (r.is_ramdisk ? t('settings.root_ramdisk') : '');
    const del = document.createElement('button');
    del.className = 'mini';
    del.textContent = t('common.delete');
    del.addEventListener('click', async function () {
      try {
        await invoke('delete_root', { id: r.id });
        closeModal();
        toast(t('settings.root_deleted'), '#' + r.id + ' ' + r.path, 'ok');
        await refresh(true);
      } catch (e) {
        toast(t('settings.root_delete_failed'), e.message, 'error');
      }
    });
    row.appendChild(a);
    row.appendChild(b);
    row.appendChild(del);
    rootList.appendChild(row);
  });
  if (!snap.roots || !snap.roots.length) rootList.textContent = t('settings.no_root');
  rootField.appendChild(rootList);
  body.appendChild(rootField);

  const addField = document.createElement('div');
  addField.className = 'field';
  const addLabel = document.createElement('label');
  addLabel.textContent = t('settings.add_root');
  addField.appendChild(addLabel);
  const row = document.createElement('div');
  row.className = 'field-row';
  const nameInput = mkInput(t('settings.name_ph'));
  const pathInput = mkInput(t('settings.path_ph'));
  const pick = mkPickButton(t('common.browse'), async function () {
    const p = await invoke('pick_folder', { title: t('settings.pick_title'), defaultPath: pathInput.value });
    if (p) pathInput.value = p;
  });
  const addBtn = document.createElement('button');
  addBtn.className = 'btn primary';
  addBtn.textContent = t('common.add');
  addBtn.addEventListener('click', async function () {
    try {
      await invoke('add_root', {
        label: nameInput.value || 'RAMDisk',
        path: pathInput.value,
        isRamdisk: true,
        waitTimeoutMs: 90000,
      });
      closeModal();
      toast(t('settings.root_added'), pathInput.value, 'ok');
      await refresh(true);
    } catch (e) {
      toast(t('settings.root_add_failed'), e.message, 'error');
    }
  });
  // 只有"路径"输入框拉满，两个按钮各自紧凑 —— 否则「浏览/添加」会被撑成一大条。
  nameInput.style.flex = '0 0 180px';
  pathInput.style.flex = '1 1 auto';
  pick.style.flex = '0 0 auto';
  addBtn.style.flex = '0 0 auto';
  row.appendChild(nameInput);
  row.appendChild(pathInput);
  row.appendChild(pick);
  row.appendChild(addBtn);
  addField.appendChild(row);
  const addHint = document.createElement('div');
  addHint.className = 'hint';
  addHint.textContent = t('settings.add_hint');
  addField.appendChild(addHint);
  body.appendChild(addField);

  openModal({
    title: t('settings.title'),
    body: body,
    wide: true,
    actions: [
      { label: t('settings.export'), onClick: function () { closeModal(); doExport(); } },
      { label: t('settings.import'), onClick: function () { closeModal(); doImport(); } },
      { label: t('common.close'), kind: 'primary', onClick: closeModal },
    ],
  });
}

function mkInput(placeholder) {
  const i = document.createElement('input');
  i.className = 'input';
  i.placeholder = placeholder;
  return i;
}

function mkSmallBtn(label, onClick, kind) {
  const b = document.createElement('button');
  b.className = 'btn' + (kind ? ' ' + kind : ' ghost');
  b.textContent = label;
  b.addEventListener('click', onClick);
  return b;
}

/** 设置页里的「开机自启」区块。 */
async function appendAutostartSection(body, snap) {
  let auto = null;
  try {
    auto = await invoke('autostart_status');
  } catch (e) {
    auto = null;
  }

  const field = document.createElement('div');
  field.className = 'field';
  const label = document.createElement('label');
  label.textContent = t('auto.label');
  field.appendChild(label);

  const box = document.createElement('div');
  box.className = 'log-list';
  const row = document.createElement('div');
  row.className = 'log-item';
  const kind = document.createElement('div');
  kind.className = 'log-kind';
  kind.textContent = !auto ? t('auto.unknown') : auto.registered ? t('auto.enabled') : t('auto.disabled');
  const msg = document.createElement('div');
  msg.className = 'log-msg';
  msg.textContent = auto ? auto.message : t('auto.read_failed');
  row.appendChild(kind);
  row.appendChild(msg);
  box.appendChild(row);

  // 第二个任务：最高权限，只重建 C:\Windows 这类受保护源
  if (auto) {
    const erow = document.createElement('div');
    erow.className = 'log-item';
    const ekind = document.createElement('div');
    ekind.className = 'log-kind';
    ekind.textContent = auto.elevated_registered ? t('auto.elev_enabled') : t('auto.elev_disabled');
    const emsg = document.createElement('div');
    emsg.className = 'log-msg';
    emsg.textContent = auto.elevated_message || t('auto.elev_hint');
    erow.appendChild(ekind);
    erow.appendChild(emsg);
    box.appendChild(erow);

    // 第三个任务：系统启动时（SYSTEM 身份），只在登录前重建系统设置模式规则的目录
    const srow = document.createElement('div');
    srow.className = 'log-item';
    const skind = document.createElement('div');
    skind.className = 'log-kind';
    skind.textContent = auto.startup_registered ? t('auto.startup_enabled') : t('auto.startup_disabled');
    const sms = document.createElement('div');
    sms.className = 'log-msg';
    sms.textContent = auto.startup_message || t('auto.startup_hint');
    srow.appendChild(skind);
    srow.appendChild(sms);
    box.appendChild(srow);
  }
  field.appendChild(box);

  const actions = document.createElement('div');
  actions.className = 'detail-actions';
  actions.appendChild(
    mkSmallBtn(t('auto.enable'), async function () {
      try {
        const cmd = await invoke('autostart_enable');
        toast(t('auto.enabled_toast'), cmd, 'ok');
        closeModal();
        await openSettings();
      } catch (e) {
        toast(t('auto.enable_failed'), e.message, 'error');
      }
    }, 'primary')
  );
  actions.appendChild(
    mkSmallBtn(t('auto.disable'), async function () {
      try {
        const msg2 = await invoke('autostart_disable');
        toast(t('auto.disabled_toast'), msg2, 'ok');
        closeModal();
        await openSettings();
      } catch (e) {
        toast(t('auto.disable_failed'), e.message, 'error');
      }
    })
  );
  actions.appendChild(
    mkSmallBtn(t('auto.run_now'), async function () {
      try {
        const msg2 = await invoke('autostart_run_now');
        toast(t('auto.triggered'), msg2, 'ok');
        setTimeout(function () { refresh(true); }, 3000);
      } catch (e) {
        toast(t('auto.trigger_failed'), e.message, 'error');
      }
    })
  );
  field.appendChild(actions);

  if (auto) {
    const elevActions = document.createElement('div');
    elevActions.className = 'detail-actions';
    if (auto.elevated_registered) {
      elevActions.appendChild(
        mkSmallBtn(t('auto.elev_disable'), async function () {
          try {
            const msg2 = await invoke('autostart_elevated_disable');
            toast(t('auto.elev_disabled_toast'), msg2, 'ok');
            closeModal();
            await openSettings();
          } catch (e) {
            toast(t('auto.elev_disable_failed'), e.message, 'error');
          }
        })
      );
    } else {
      elevActions.appendChild(
        mkSmallBtn(t('auto.elev_enable'), async function () {
          try {
            const msg2 = await invoke('autostart_elevated_enable');
            toast(t('auto.elev_enabled_toast'), msg2, 'ok');
            closeModal();
            await openSettings();
          } catch (e) {
            toast(t('auto.elev_enable_failed'), e.message, 'error');
          }
        })
      );
    }
    field.appendChild(elevActions);

    // 系统启动时任务（SYSTEM 身份）：登录前把系统设置模式规则的目标目录建出来
    const startupActions = document.createElement('div');
    startupActions.className = 'detail-actions';
    if (auto.startup_registered) {
      startupActions.appendChild(
        mkSmallBtn(t('auto.startup_disable'), async function () {
          try {
            const msg2 = await invoke('autostart_startup_disable');
            toast(t('auto.startup_disabled_toast'), msg2, 'ok');
            closeModal();
            await openSettings();
          } catch (e) {
            toast(t('auto.startup_disable_failed'), e.message, 'error');
          }
        })
      );
    } else {
      startupActions.appendChild(
        mkSmallBtn(t('auto.startup_enable'), async function () {
          try {
            const msg2 = await invoke('autostart_startup_enable');
            toast(t('auto.startup_enabled_toast'), msg2, 'ok');
            closeModal();
            await openSettings();
          } catch (e) {
            toast(t('auto.startup_enable_failed'), e.message, 'error');
          }
        })
      );
    }
    field.appendChild(startupActions);
  }

  const hint = document.createElement('div');
  hint.className = 'hint';
  if (auto) {
    const parts = [
      tf('auto.p1', { name: auto.task_name }),
      tf('auto.p2', { cmd: (auto.command || auto.recommended_exe + ' boot-rebuild') }),
      tf('auto.p3', { id: auto.elevated ? t('auto.admin') : t('auto.normal') }),
      tf('auto.p5', { name: auto.elevated_task_name }),
      tf('auto.p6', { name: auto.startup_task_name }),
    ];
    if (auto.extra_args) {
      parts.push(tf('auto.p4', { args: auto.extra_args }));
    }
    if (auto.needs_update) {
      parts.push(t('auto.needs_update'));
    }
    hint.textContent = parts.join(' ');
  } else {
    hint.textContent = '';
  }
  field.appendChild(hint);
  body.appendChild(field);
}

function mkPickButton(label, onClick) {
  const b = document.createElement('button');
  b.className = 'btn';
  b.textContent = label;
  b.addEventListener('click', onClick);
  return b;
}

async function doExport() {
  try {
    const path = await invoke('pick_save_file', { title: t('io.export_title'), defaultName: 'sublink-config.json' });
    if (!path) return;
    const r = await invoke('export_config', { path: path });
    toast(t('io.exported'), tf('io.exported_sub', { roots: r[0], rules: r[1], path: path }), 'ok');
  } catch (e) {
    toast(t('io.export_failed'), e.message, 'error');
  }
}

async function doImport() {
  try {
    const path = await invoke('pick_open_file', { title: t('io.import_title') });
    if (!path) return;
    const rep = await invoke('import_config', { path: path, ramRoot: null, dryRun: false });
    const msg = tf('io.import_msg', { roots: rep.roots_created, rules: rep.rules_created, skipped: rep.rules_skipped }) +
      (rep.rules_hidden ? tf('io.import_hidden', { n: rep.rules_hidden }) : '') +
      (rep.errors.length ? tf('io.import_errors', { n: rep.errors.length }) : '');
    toast(t('io.imported'), msg, rep.errors.length ? 'warn' : 'ok');
    if (rep.errors.length) {
      const body = document.createElement('div');
      rep.errors.forEach(function (e) {
        const d = document.createElement('div');
        d.className = 'hint warn';
        d.textContent = '⚠ ' + e;
        body.appendChild(d);
      });
      openModal({ title: t('io.import_problems'), body: body, actions: [{ label: t('act.boot.got_it'), kind: 'primary', onClick: closeModal }] });
    }
    await refresh(true);
  } catch (e) {
    toast(t('io.import_failed'), e.message, 'error');
  }
}

// ---------------------------------------------------------------------------
// 规则行内编辑（列表单元格即表单，不再弹编辑对话框）
// ---------------------------------------------------------------------------

/** 当前正在编辑的单元格（同一时刻只允许一个）。 */
let activeCellEditor = null;
/** 编辑另一格时，先把当前格提交，成功/取消后自动打开这一格。 */
let pendingCell = null;
/** 当前格提交后要载入的模板（点模板行时先把正在编辑的单元格保存掉）。 */
let pendingTemplate = null;
/** 当前格提交后要复制的规则（右键「复制为新规则」时先保存正在编辑的单元格）。 */
let pendingDraftRule = null;
/** 行内保存进行中：此时刷新/重画会吞掉焦点，先拒绝新的编辑动作。 */
let inlineSaving = false;

function cellInput(value, placeholder, type) {
  const i = document.createElement('input');
  i.className = 'cell-input';
  i.type = type || 'text';
  if (value !== null && value !== undefined) i.value = String(value);
  if (placeholder) i.placeholder = placeholder;
  return i;
}

function cellSelect(pairs, value) {
  const s = document.createElement('select');
  s.className = 'cell-select';
  pairs.forEach(function (p) {
    const o = document.createElement('option');
    o.value = p[0];
    o.textContent = p[1];
    s.appendChild(o);
  });
  if (value !== undefined && value !== null) s.value = String(value);
  return s;
}

function cellRow() {
  const d = document.createElement('div');
  d.className = 'cell-editor-row';
  for (let i = 0; i < arguments.length; i++) {
    if (arguments[i]) d.appendChild(arguments[i]);
  }
  return d;
}

/** 水位策略的短描述（单元格里空间有限，详情面板仍用完整措辞）。 */
function autoPolicyShort(policy) {
  if (!policy) return t('auto.short.off');
  if (policy === 'empty') return t('auto.short.empty');
  const m = /^older:(\d+)$/.exec(policy);
  if (m) {
    const secs = Number(m[1]);
    if (secs % 86400 === 0) return tf('auto.short.older_days', { n: secs / 86400 });
    return tf('auto.short.older_hours', { n: Math.max(1, Math.round(secs / 3600)) });
  }
  const k = /^keep:(\d+)$/.exec(policy);
  if (k) return tf('auto.short.keep', { size: humanBytes(Number(k[1])) });
  return policy;
}

function parseAutoPolicy(policy) {
  const p = policy || '';
  if (p === 'empty') return { mode: 'empty', hours: 24, num: 0, unit: 'mb' };
  const m = /^older:(\d+)$/.exec(p);
  if (m) return { mode: 'older', hours: Math.max(1, Math.round(Number(m[1]) / 3600)), num: 0, unit: 'mb' };
  const k = /^keep:(\d+)$/.exec(p);
  if (k) {
    const bytes = Number(k[1]);
    if (bytes >= 1073741824 && bytes % 1073741824 === 0) {
      return { mode: 'keep', hours: 24, num: bytes / 1073741824, unit: 'gb' };
    }
    return { mode: 'keep', hours: 24, num: Math.max(1, Math.round(bytes / 1048576)), unit: 'mb' };
  }
  return { mode: '', hours: 24, num: 0, unit: 'mb' };
}

function ruleFormOf(rule, patch) {
  const pick = function (k, fallback) {
    return patch[k] !== undefined ? patch[k] : fallback;
  };
  return {
    name: pick('name', rule.name),
    category: pick('category', rule.category),
    source_template: pick('source_template', rule.source_template),
    target_root_id: pick('target_root_id', rule.target_root_id),
    target_subpath: pick('target_subpath', rule.target_subpath),
    link_type: pick('link_type', rule.link_type),
    redirect_mode: pick('redirect_mode', rule.redirect_mode || 'link'),
    env_vars: pick('env_vars', rule.env_vars || []),
    env_scope: pick('env_scope', rule.env_scope || 'user'),
    want_enabled: pick('want_enabled', rule.want_enabled),
    rebuild_on_boot: pick('rebuild_on_boot', rule.rebuild_on_boot),
    notes: pick('notes', rule.notes || null),
    excludes: pick('excludes', rule.excludes || []),
    auto_clean_policy: pick('auto_clean_policy', rule.auto_clean_policy || ''),
    auto_clean_limit_bytes: pick('auto_clean_limit_bytes', rule.auto_clean_limit_bytes || 0),
  };
}

function validateRuleValues(form) {
  if (!form.name) throw new Error(t('inline.name_required'));
  if (!form.source_template) throw new Error(t('inline.source_required'));
  if (!form.target_subpath) throw new Error(t('inline.sub_required'));
  if (form.auto_clean_limit_bytes < 0) throw new Error(t('inline.bad_limit'));
  if (form.auto_clean_limit_bytes > 0 && !form.auto_clean_policy) {
    throw new Error(t('rule.auto.limit_needs_policy'));
  }
  // 「系统设置模式」至少要有一个变量名（后端也有同样的白名单校验）。
  if (form.redirect_mode === 'setting' && !(form.env_vars || []).length) {
    throw new Error(t('sysenv.no_vars'));
  }
}

function sameRuleValue(a, b) {
  if (Array.isArray(a) || Array.isArray(b)) {
    return JSON.stringify(a || []) === JSON.stringify(b || []);
  }
  const emptyA = a === null || a === undefined || a === '';
  const emptyB = b === null || b === undefined || b === '';
  if (emptyA && emptyB) return true;
  return a === b;
}

function patchChanged(rule, patch) {
  return Object.keys(patch).some(function (k) {
    return !sameRuleValue(rule[k], patch[k]);
  });
}

/** 按字段生成单元格里的编辑控件。src 可以是规则视图，也可以是新增草稿。 */
function buildFieldEditor(field, src) {
  const box = document.createElement('div');
  box.className = 'cell-editor';
  let focus = null;
  let read = function () { return null; };
  /** 可选的异步校验：返回错误文案，null 表示可以保存。 */
  let check = null;

  if (field === 'name' || field === 'notes') {
    const inp = cellInput(
      field === 'name' ? src.name : src.notes,
      field === 'name' ? t('rule.name_ph') : t('rule.notes_ph')
    );
    box.appendChild(inp);
    focus = inp;
    read = function () {
      if (field === 'name') return { name: inp.value.trim() };
      return { notes: inp.value.trim() || null };
    };
  } else if (field === 'source') {
    const inp = cellInput(src.source_template, t('inline.source_ph'));
    inp.spellcheck = false;
    const hint = document.createElement('div');
    hint.className = 'cell-hint';
    box.appendChild(inp);
    box.appendChild(hint);
    focus = inp;
    let seq = 0;
    let wait = null;

    /** 行内提示源路径展开结果；阻断路径返回错误文案（保存会被拦下）。 */
    async function runCheck() {
      if (wait) {
        clearTimeout(wait);
        wait = null;
      }
      const value = inp.value.trim();
      const my = ++seq;
      if (!value) {
        hint.textContent = '';
        hint.className = 'cell-hint';
        hint.title = '';
        return null;
      }
      try {
        const info = await invoke('inspect_source', { template: value });
        if (my !== seq) return null;
        if (info.blocking && info.blocking.length) {
          hint.textContent = '⛔ ' + info.blocking.join('；');
          hint.className = 'cell-hint err';
          hint.title = hint.textContent;
          return info.blocking.join('；');
        }
        const parts = [
          tf('rule.src.expanded', { path: info.expanded }) +
            (info.exists ? t('rule.src.exists') : t('rule.src.not_exist')),
        ];
        if (info.unknown_vars && info.unknown_vars.length) {
          parts.push(tf('rule.src.unknown', { vars: info.unknown_vars.join(t('list.sep')) }));
        }
        if (info.requires_elevation) parts.push(t('rule.src.elev'));
        hint.textContent = parts.join(' · ');
        hint.className = 'cell-hint ' +
          ((info.unknown_vars && info.unknown_vars.length) || info.requires_elevation ? 'warn' : 'ok');
        hint.title = hint.textContent;
        return null;
      } catch (e) {
        if (my !== seq) return null;
        // 校验命令本身失败时不阻断保存，交给 update_rule 的最终校验。
        hint.textContent = e.message;
        hint.className = 'cell-hint warn';
        hint.title = hint.textContent;
        return null;
      }
    }

    inp.addEventListener('input', function () {
      if (wait) clearTimeout(wait);
      hint.textContent = t('inline.checking');
      hint.className = 'cell-hint';
      wait = setTimeout(runCheck, 350);
    });
    read = function () { return { source_template: inp.value.trim() }; };
    check = runCheck;
    if (inp.value.trim()) runCheck();
  } else if (field === 'category') {
    const sel = cellSelect((state.snap.categories || []).map(function (c) {
      return [c, categoryLabel(c)];
    }), src.category);
    box.appendChild(sel);
    focus = sel;
    read = function () { return { category: sel.value }; };
  } else if (field === 'link_type') {
    const sel = cellSelect([
      ['junction', t('inline.link_junction')],
      ['symlink_dir', t('inline.link_symlink')],
    ], src.link_type);
    box.appendChild(sel);
    focus = sel;
    read = function () { return { link_type: sel.value }; };
  } else if (field === 'redirect_mode') {
    // 重定向方式：链接（建 Junction）/ 系统设置（改环境变量）。
    // 选「系统设置」时展开变量名与作用域两栏；选「链接」时展开链接类型。
    //
    // 「系统设置」只对"源路径正好是某个 TEMP/TMP 当前指向的目录"的规则可实现：
    // 这个模式改的是注册表里的环境变量值，变量指向别处就落不了地
    // （后端 apply 会以 source_mismatch 拒掉）。所以对不可实现的规则，
    // 下拉里干脆不给这个选项，并在下面说明原因。
    const settingOptions = src.setting_options || [];
    const canSetting = settingOptions.length > 0;
    const curMode = src.redirect_mode || 'link';
    const modePairs = [['link', t('inline.mode_link')]];
    if (canSetting) modePairs.push(['setting', t('inline.mode_setting')]);
    // 兜底：当前值不在候选里（存量规则被外部改过）时补一条，
    // 免得下拉变空、一保存就把规则静默降级成链接模式。
    if (!modePairs.some(function (p) { return p[0] === curMode; })) {
      modePairs.push([curMode, t('inline.mode_setting')]);
    }
    const mode = cellSelect(modePairs, curMode);
    const linkSel = cellSelect([
      ['junction', t('inline.link_junction')],
      ['symlink_dir', t('inline.link_symlink')],
    ], src.link_type || 'junction');
    // 变量名与作用域的默认值取第一个可行组合（后端按注册表实际值算出来的），
    // 而不是写死 TEMP,TMP / user —— 机器级和用户级的组合不一样。
    const first = settingOptions[0] || null;
    const defVars = first
      ? first.vars.join(',')
      : ((src.env_vars && src.env_vars.length) ? src.env_vars.join(',') : 'TEMP,TMP');
    const defScope = first ? first.scope : (src.env_scope || 'user');
    const varsSel = cellSelect([
      ['TEMP,TMP', 'TEMP + TMP'],
      ['TEMP', 'TEMP'],
      ['TMP', 'TMP'],
    ], defVars);
    const scopeSel = cellSelect([
      ['user', t('sysenv.scope.user')],
      ['machine', t('sysenv.scope.machine')],
    ], defScope);
    const linkBox = cellRow(linkSel);
    const varBox = cellRow(varsSel, scopeSel);
    const hint = document.createElement('div');
    hint.className = 'cell-hint';
    if (canSetting) {
      hint.textContent = t('inline.setting_available');
    } else {
      hint.textContent = t('inline.setting_unavailable');
    }
    box.appendChild(mode);
    box.appendChild(linkBox);
    box.appendChild(varBox);
    box.appendChild(hint);
    focus = mode;
    function syncMode() {
      const isSetting = mode.value === 'setting';
      linkBox.style.display = isSetting ? 'none' : '';
      varBox.style.display = isSetting ? '' : 'none';
      // 提示分两种：不可用时**始终**显示（它解释的是"为什么没有这个选项"）；
      // 可用时只在链接模式下显示 —— 选了系统设置，两栏里已经写着变量与作用域。
      hint.style.display = (isSetting && canSetting) ? 'none' : '';
    }
    mode.addEventListener('change', syncMode);
    syncMode();
    read = function () {
      if (mode.value === 'setting') {
        return {
          redirect_mode: 'setting',
          env_vars: varsSel.value.split(',').map(function (s) { return s.trim(); }).filter(Boolean),
          env_scope: scopeSel.value,
        };
      }
      return { redirect_mode: 'link', link_type: linkSel.value, env_vars: [], env_scope: 'user' };
    };
  } else if (field === 'target') {
    const root = cellSelect((state.snap.roots || []).map(function (r) {
      return [String(r.id), r.label + ' · ' + r.path];
    }), String(src.target_root_id));
    const sub = cellInput(src.target_subpath, t('rule.sub_ph'));
    box.appendChild(root);
    box.appendChild(sub);
    focus = sub;
    read = function () {
      return { target_root_id: Number(root.value), target_subpath: sub.value.trim() };
    };
  } else if (field === 'clean_policy') {
    const parsed = parseAutoPolicy(src.auto_clean_policy);
    const sel = cellSelect([
      ['', t('auto.short.off')],
      ['empty', t('auto.short.empty')],
      ['older', t('auto.short.older')],
      ['keep', t('auto.short.keep_field')],
    ], parsed.mode);
    const num = cellInput(parsed.mode === 'older' ? parsed.hours : (parsed.mode === 'keep' ? parsed.num : ''), '', 'number');
    num.min = '1';
    num.step = '1';
    const unit = cellSelect([
      ['mb', t('rule.auto.unit_mb')],
      ['gb', t('rule.auto.unit_gb')],
    ], parsed.unit);
    const param = cellRow(num, unit);
    box.appendChild(sel);
    box.appendChild(param);
    focus = sel;
    function syncMode() {
      param.style.display = (sel.value === 'older' || sel.value === 'keep') ? '' : 'none';
      unit.style.display = sel.value === 'keep' ? '' : 'none';
      num.placeholder = sel.value === 'older' ? t('rule.auto.hours_ph') : t('rule.auto.keep_ph');
      // 切到按时间 / 按空间时给一个可用的起点值，避免“选了策略却没填值”被静默存成未启用。
      if (!num.value) {
        if (sel.value === 'older') num.value = '24';
        else if (sel.value === 'keep') num.value = '100';
      }
    }
    sel.addEventListener('change', syncMode);
    syncMode();
    read = function () {
      if (sel.value === 'empty') return { auto_clean_policy: 'empty' };
      const n = Math.floor(Number(num.value));
      if (sel.value === 'older') {
        return { auto_clean_policy: n > 0 ? 'older:' + (n * 3600) : '' };
      }
      if (sel.value === 'keep') {
        if (!(n > 0)) return { auto_clean_policy: '' };
        const bytes = unit.value === 'gb' ? n * 1073741824 : n * 1048576;
        return { auto_clean_policy: 'keep:' + bytes };
      }
      return { auto_clean_policy: '' };
    };
  } else if (field === 'clean_limit') {
    const bytes0 = Number(src.auto_clean_limit_bytes) || 0;
    let num0 = 0;
    let unit0 = 'mb';
    if (bytes0 >= 1073741824 && bytes0 % 1073741824 === 0) {
      num0 = bytes0 / 1073741824;
      unit0 = 'gb';
    } else if (bytes0 > 0) {
      num0 = Math.max(1, Math.round(bytes0 / 1048576));
      unit0 = 'mb';
    }
    const num = cellInput(num0 > 0 ? num0 : '', t('rule.auto.limit_ph'), 'number');
    num.min = '0';
    num.step = '1';
    const unit = cellSelect([
      ['mb', t('rule.auto.unit_mb')],
      ['gb', t('rule.auto.unit_gb')],
    ], unit0);
    box.appendChild(cellRow(num, unit));
    focus = num;
    read = function () {
      const n = Math.floor(Number(num.value));
      if (!(n > 0)) return { auto_clean_limit_bytes: 0 };
      return { auto_clean_limit_bytes: unit.value === 'gb' ? n * 1073741824 : n * 1048576 };
    };
  } else if (field === 'excludes') {
    const ta = document.createElement('textarea');
    ta.className = 'cell-input cell-textarea';
    ta.rows = 2;
    ta.placeholder = t('inline.excludes_ph');
    ta.value = (src.excludes || []).join('\n');
    box.appendChild(ta);
    focus = ta;
    read = function () {
      return {
        excludes: ta.value.split(/\r?\n/).map(function (s) { return s.trim(); })
          .filter(function (s) { return s.length > 0; }),
      };
    };
  }

  return { node: box, focus: focus || box, read: read, check: check };
}

/** 已建链接的规则改了源/目标/链接类型后，需要重新应用才能生效。 */
function isLinkedRule(rule) {
  return rule.badge === '✅' || rule.badge === '⚠️';
}

/** 改源路径后必须由用户重新应用（可能涉及搬运数据，不能静默做）。 */
function needsReapply(rule, patch) {
  if (!isLinkedRule(rule)) return false;
  if (patch.source_template !== undefined && !sameRuleValue(rule.source_template, patch.source_template)) return true;
  // 「系统设置模式」改了目标/变量/作用域 → 环境变量要重新指向新目标，
  // 不能静默做（涉及搬数据），提示用户重新应用。
  if (rule.redirect_mode === 'setting') {
    return ['target_root_id', 'target_subpath', 'redirect_mode', 'env_vars', 'env_scope'].some(function (k) {
      return patch[k] !== undefined && !sameRuleValue(rule[k], patch[k]);
    });
  }
  return false;
}

/** 改目标 / 链接类型：后端会自动删掉源路径上的旧链接，并按新目标重建。 */
function autoRetargets(rule, patch) {
  if (!isLinkedRule(rule)) return false;
  if (patch.source_template !== undefined && !sameRuleValue(rule.source_template, patch.source_template)) return false;
  // 「系统设置模式」没有链接可重建 —— 改了目标/变量必须由用户重新应用。
  if (rule.redirect_mode === 'setting') return false;
  return ['target_root_id', 'target_subpath', 'link_type'].some(function (k) {
    return patch[k] !== undefined && !sameRuleValue(rule[k], patch[k]);
  });
}

/** 单个单元格保存：合并完整表单后调用 update_rule（后端要求完整 RuleForm）。 */
async function saveRulePatch(rule, patch) {
  const form = ruleFormOf(rule, patch);
  validateRuleValues(form);
  if (!patchChanged(rule, patch)) {
    renderRows();
    return false;
  }
  const reapply = needsReapply(rule, patch);
  const retarget = !reapply && autoRetargets(rule, patch);
  inlineSaving = true;
  try {
    await invoke('update_rule', { id: rule.id, form: form });
    if (reapply) toast(t('toast.reapply_title'), t('toast.reapply_msg'), 'warn');
    else if (retarget) toast(t('toast.retargeted_title'), t('toast.retargeted_msg'), 'ok');
    else toast(t('common.saved'), form.name || ('#' + rule.id), 'ok');
    await refresh(true);
    return true;
  } catch (e) {
    toast(t('common.save_failed'), e.message, 'error');
    await refresh(true);
    return false;
  } finally {
    inlineSaving = false;
  }
}

/** 把一个单元格切换成编辑态。回车 / 失焦保存，Esc 撤销。 */
function beginCellEdit(td, rule, field) {
  if (inlineSaving || !td || !rule) return;
  if (activeCellEditor) {
    // 另一格还在编辑：先把那一格提交，避免两套输入控件抢焦点。
    activeCellEditor.commit();
    return;
  }

  td.classList.add('editing');
  td.textContent = '';
  const editor = buildFieldEditor(field, rule);
  td.appendChild(editor.node);

  let done = false;
  let busy = false;
  let timer = null;

  function cleanup() {
    if (timer) clearTimeout(timer);
    activeCellEditor = null;
    td.classList.remove('editing');
  }

  function cancel() {
    if (done) return;
    done = true;
    cleanup();
    renderRows();
    runPending(rule.id, field);
  }

  async function doCommit() {
    let patch = editor.read();
    if (!patch) {
      cancel();
      return;
    }
    let form = ruleFormOf(rule, patch);

    function handleValidation(e) {
      const limitProblem = form.auto_clean_limit_bytes > 0 && !form.auto_clean_policy;
      if (limitProblem && field === 'clean_limit') {
        toast(t('common.save_failed'), t('inline.limit_needs_policy_hint'), 'error');
        pendingCell = null;
        pendingTemplate = null;
        pendingDraftRule = null;
        cancel();
        openCellEditor(rule.id, 'clean_policy', true);
        return true;
      }
      if (limitProblem && field === 'clean_policy') {
        toast(t('common.save_failed'), t('inline.policy_needs_limit_hint'), 'error');
        pendingCell = null;
        pendingTemplate = null;
        pendingDraftRule = null;
        cancel();
        openCellEditor(rule.id, 'clean_limit', true);
        return true;
      }
      // 其它校验不过就留在编辑态，别把用户刚输入的内容清掉；这一次点击不再跳格。
      pendingCell = null;
      pendingTemplate = null;
      pendingDraftRule = null;
      toast(t('common.save_failed'), e.message, 'error');
      editor.focus.focus();
      return true;
    }

    try {
      validateRuleValues(form);
    } catch (e) {
      if (handleValidation(e)) return;
    }

    if (editor.check) {
      const problem = await editor.check();
      // 校验是异步的：等待期间用户可能按了 Esc / 点了别处，已经关闭这一格。
      if (done) return;
      if (problem) {
        pendingCell = null;
        pendingTemplate = null;
        pendingDraftRule = null;
        toast(t('common.save_failed'), problem, 'error');
        editor.focus.focus();
        return;
      }
      // 校验期间用户可能又改了源路径：以最新值为准重新校验一次。
      patch = editor.read();
      if (!patch) {
        cancel();
        return;
      }
      form = ruleFormOf(rule, patch);
      try {
        validateRuleValues(form);
      } catch (e) {
        if (handleValidation(e)) return;
      }
    }

    if (!patchChanged(rule, patch)) {
      cancel();
      return;
    }
    done = true;
    cleanup();
    await saveRulePatch(rule, patch);
    runPending(rule.id, field);
  }

  function commit() {
    if (done || busy) return Promise.resolve();
    busy = true;
    return doCommit().finally(function () { busy = false; });
  }

  activeCellEditor = { commit: commit, cancel: cancel };
  editor.focus.focus();
  if (editor.focus.tagName === 'INPUT' && editor.focus.select) editor.focus.select();

  editor.node.addEventListener('keydown', function (ev) {
    if (ev.key === 'Enter' && ev.target.tagName !== 'TEXTAREA') {
      ev.preventDefault();
      commit();
    } else if (ev.key === 'Escape') {
      ev.preventDefault();
      ev.stopPropagation();
      cancel();
    }
  });
  editor.node.addEventListener('focusout', function (ev) {
    if (done) return;
    if (ev.relatedTarget && editor.node.contains(ev.relatedTarget)) return;
    if (timer) clearTimeout(timer);
    // 等一拍再判断焦点去了哪里：组合控件（目标根 + 子目录）内部切换不算失焦。
    timer = setTimeout(function () {
      if (done) return;
      if (editor.node.contains(document.activeElement)) return;
      commit();
    }, 60);
  });
}

/** 旗标单击即切换并保存（不需要输入框）。 */
async function toggleRuleFlag(rule, field) {
  if (inlineSaving || !rule) return;
  if (activeCellEditor) {
    activeCellEditor.commit();
    return;
  }
  const patch = {};
  patch[field] = !rule[field];
  try {
    await saveRulePatch(rule, patch);
  } catch (e) {
    toast(t('common.save_failed'), e.message, 'error');
  }
}

/** 重画后按 id + 字段打开某行的行内编辑器（详情编辑、依赖引导共用）。 */
function openCellEditor(id, field, flash) {
  const rule = (state.snap.rules || []).find(function (r) { return r.id === id; });
  if (!rule) return;
  state.focusId = id;
  if (!state.treeOpen.has(rule.category)) state.treeOpen.add(rule.category);
  renderSidebar();
  renderRows();

  const tr = document.querySelector('#rows tr[data-id="' + id + '"]');
  if (!tr) return;
  tr.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  const td = tr.querySelector('td[data-field="' + field + '"]');
  if (!td) return;
  if (flash) {
    td.classList.add('need-attention');
    setTimeout(function () { td.classList.remove('need-attention'); }, 2400);
  }
  beginCellEdit(td, rule, field);
}

/** 上一格提交完后的收尾：打开刚点的新格 / 载入刚点的模板 / 把焦点还给原格。 */
function runPending(ownId, ownField) {
  if (pendingCell) {
    const next = pendingCell;
    pendingCell = null;
    openCellEditor(next.id, next.field);
    return;
  }
  if (pendingTemplate) {
    const preset = pendingTemplate;
    pendingTemplate = null;
    startDraftRule(preset);
    return;
  }
  if (pendingDraftRule) {
    const rule = pendingDraftRule;
    pendingDraftRule = null;
    startDraftFromRule(rule);
    return;
  }
  if (ownId !== undefined) refocusCell(ownId, ownField);
}

/** 保存后把焦点还给同一格，键盘用户可以继续按 Enter / F2 编辑。 */
function refocusCell(id, field) {
  const td = document.querySelector('#rows tr[data-id="' + id + '"] td[data-field="' + field + '"]');
  if (td) td.focus({ preventScroll: true });
}

/** 详情面板 / 右键菜单的「编辑」：滚到该行并直接打开名称单元格。 */
function focusInlineEdit(id) {
  openCellEditor(id, 'name');
}

/** 列表顶部插入一条空白草稿行，所有字段直接在行内填。 */
/** 列表顶部插入草稿行；传入 preset 时用内置模板预填（仍然只填草稿，不直接建规则）。 */
function startDraftRule(preset) {
  if (!state.snap) return;
  if (!(state.snap.roots || []).length) {
    toast(t('rule.no_root_title'), t('rule.no_root_hint'), 'warn');
    return;
  }
  // 再点一次「添加规则」只是把焦点带回草稿；点模板才用模板覆盖草稿。
  if (!preset && state.draft) {
    focusDraft('name');
    return;
  }
  const cats = state.snap.categories || ['other'];
  const category = preset && cats.indexOf(preset.category) >= 0
    ? preset.category
    : (cats.indexOf('other') >= 0 ? 'other' : cats[0]);
  state.draft = {
    name: preset ? (preset.name || '') : '',
    category: category,
    source_template: preset ? (preset.source_template || '') : '',
    target_root_id: state.snap.roots[0].id,
    target_subpath: preset ? (preset.target_subpath || '') : '',
    link_type: 'junction',
    redirect_mode: preset ? (preset.redirect_mode || 'link') : 'link',
    env_vars: preset ? (preset.env_vars || []) : [],
    env_scope: preset ? (preset.env_scope || 'user') : 'user',
    // 「系统设置」可不可用，由后端按注册表里 TEMP/TMP 的实际指向算出来。
    setting_options: preset ? (preset.setting_options || []) : [],
    want_enabled: true,
    rebuild_on_boot: true,
    notes: preset ? (preset.note || null) : null,
    excludes: [],
    auto_clean_policy: '',
    auto_clean_limit_bytes: 0,
  };
  renderRows();
  focusDraft('name');
  if (preset) toast(t('tpl.draft_loaded'), preset.name, 'ok');
}

/** 右键「复制为新规则」的入口：先把正在编辑的单元格提交，再载入草稿。 */
function duplicateRuleToDraft(rule) {
  if (inlineSaving || !rule) return;
  if (activeCellEditor) {
    pendingDraftRule = rule;
    activeCellEditor.commit();
    return;
  }
  startDraftFromRule(rule);
}

/** 把一条已有规则的可编辑字段完整带进顶部草稿行；名称加副本后缀。
    源路径沿用原值（保存前需改成不重复的路径，否则后端会拒绝）。 */
function startDraftFromRule(rule) {
  if (!state.snap) return;
  if (!(state.snap.roots || []).length) {
    toast(t('rule.no_root_title'), t('rule.no_root_hint'), 'warn');
    return;
  }
  const cats = state.snap.categories || ['other'];
  const category = cats.indexOf(rule.category) >= 0
    ? rule.category
    : (cats.indexOf('other') >= 0 ? 'other' : cats[0]);
  const root = (state.snap.roots || []).find(function (r) { return r.id === rule.target_root_id; });
  state.draft = {
    name: tf('draft.copy_name', { name: rule.name }),
    category: category,
    source_template: rule.source_template,
    target_root_id: root ? root.id : state.snap.roots[0].id,
    target_subpath: rule.target_subpath,
    link_type: rule.link_type || 'junction',
    redirect_mode: rule.redirect_mode || 'link',
    env_vars: (rule.env_vars || []).slice(),
    env_scope: rule.env_scope || 'user',
    setting_options: (rule.setting_options || []).slice(),
    want_enabled: !!rule.want_enabled,
    rebuild_on_boot: !!rule.rebuild_on_boot,
    notes: rule.notes || null,
    excludes: (rule.excludes || []).slice(),
    auto_clean_policy: rule.auto_clean_policy || '',
    auto_clean_limit_bytes: rule.auto_clean_limit_bytes || 0,
  };
  renderRows();
  focusDraft('source');
  toast(t('draft.copy_loaded'), t('draft.copy_hint'), 'ok');
}

function focusDraft(field) {
  const root = document.querySelector('tr.draft-row [data-draft-field="' + field + '"]');
  if (!root) return;
  const box = root.matches('input, select, textarea') ? root : root.querySelector('input, select, textarea');
  if (!box) return;
  box.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  box.focus();
  if (box.select) box.select();
}

/** 草稿行的依赖提示：闪一下需要先填的那一格。 */
function flashDraftField(field) {
  const root = document.querySelector('tr.draft-row [data-draft-field="' + field + '"]');
  const cell = root && root.closest('td');
  if (!cell) return;
  cell.classList.add('need-attention');
  setTimeout(function () { cell.classList.remove('need-attention'); }, 2400);
}

/** 草稿行：列结构与正式规则一致，只是每格都是编辑控件。 */
function renderDraftRow() {
  const draft = state.draft;
  const tr = document.createElement('tr');
  tr.className = 'draft-row';
  tr.dataset.draft = '1';

  const tdCheck = document.createElement('td');
  const plus = document.createElement('span');
  plus.className = 'draft-plus';
  plus.textContent = '＋';
  tdCheck.appendChild(plus);
  tr.appendChild(tdCheck);

  const tdBadge = document.createElement('td');
  const badge = document.createElement('span');
  badge.className = 'badge tpl';
  badge.textContent = t('draft.badge');
  tdBadge.appendChild(badge);
  tr.appendChild(tdBadge);

  const controls = [];
  function fieldTd(field, cls) {
    const td = document.createElement('td');
    td.className = 'cell-edit editing' + (cls ? ' ' + cls : '');
    const editor = buildFieldEditor(field, draft);
    editor.node.dataset.draftField = field;
    td.appendChild(editor.node);
    tr.appendChild(td);
    controls.push({ field: field, editor: editor });
    function sync() {
      const patch = editor.read();
      if (!patch) return;
      Object.keys(patch).forEach(function (k) { draft[k] = patch[k]; });
    }
    editor.node.addEventListener('input', sync);
    editor.node.addEventListener('change', sync);
    return td;
  }

  fieldTd('name', 'cell-name-wrap');
  fieldTd('category');
  fieldTd('redirect_mode', 'cell-link');
  fieldTd('source', 'cell-source');
  fieldTd('target', 'cell-target');

  const tdUsage = document.createElement('td');
  tdUsage.className = 'cell-usage none';
  tdUsage.textContent = '—';
  tr.appendChild(tdUsage);

  const tdFlags = document.createElement('td');
  const flags = document.createElement('div');
  flags.className = 'row-flags';
  [
    { key: 'want_enabled', on: '●', off: '○', onTip: t('row.expect_on'), offTip: t('row.expect_off') },
    { key: 'rebuild_on_boot', on: '⟳', off: '·', onTip: t('row.rebuild_on'), offTip: t('row.rebuild_off') },
  ].forEach(function (f) {
    const s = document.createElement('span');
    s.className = 'toggle ' + (draft[f.key] ? 'on' : 'off');
    s.textContent = draft[f.key] ? f.on : f.off;
    s.title = draft[f.key] ? f.onTip : f.offTip;
    s.classList.add('clickable');
    s.addEventListener('click', function () {
      draft[f.key] = !draft[f.key];
      s.className = 'toggle ' + (draft[f.key] ? 'on' : 'off') + ' clickable';
      s.textContent = draft[f.key] ? f.on : f.off;
      s.title = draft[f.key] ? f.onTip : f.offTip;
    });
    flags.appendChild(s);
  });
  tdFlags.appendChild(flags);
  tr.appendChild(tdFlags);

  fieldTd('clean_policy', 'cell-auto');
  fieldTd('clean_limit', 'cell-limit');
  fieldTd('excludes', 'cell-excludes');
  fieldTd('notes', 'cell-notes');

  const tdAct = document.createElement('td');
  const wrap = document.createElement('div');
  wrap.className = 'row-actions';

  async function saveDraft() {
    function syncDraft() {
      controls.forEach(function (c) {
        const patch = c.editor.read();
        if (!patch) return;
        Object.keys(patch).forEach(function (k) { draft[k] = patch[k]; });
      });
    }
    function draftForm() {
      return {
        name: draft.name,
        category: draft.category,
        source_template: draft.source_template,
        target_root_id: draft.target_root_id,
        target_subpath: draft.target_subpath,
        link_type: draft.link_type,
        redirect_mode: draft.redirect_mode || 'link',
        env_vars: draft.env_vars || [],
        env_scope: draft.env_scope || 'user',
        want_enabled: draft.want_enabled,
        rebuild_on_boot: draft.rebuild_on_boot,
        notes: draft.notes || null,
        excludes: draft.excludes || [],
        auto_clean_policy: draft.auto_clean_policy || '',
        auto_clean_limit_bytes: draft.auto_clean_limit_bytes || 0,
      };
    }
    function limitProblem(form) {
      return form.auto_clean_limit_bytes > 0 && !form.auto_clean_policy;
    }

    syncDraft();
    const srcCtl = controls.find(function (c) { return c.field === 'source'; });
    if (srcCtl && srcCtl.editor.check) {
      const problem = await srcCtl.editor.check();
      if (problem) {
        toast(t('rule.add_failed'), problem, 'error');
        return;
      }
    }
    // 校验可能等待过，重新读一遍控件，避免把旧值写进规则。
    syncDraft();
    const form = draftForm();
    try {
      validateRuleValues(form);
    } catch (e) {
      if (limitProblem(form)) {
        toast(t('rule.add_failed'), t('inline.limit_needs_policy_hint'), 'error');
        flashDraftField('clean_policy');
        focusDraft('clean_policy');
        return;
      }
      toast(t('rule.add_failed'), e.message, 'error');
      return;
    }
    try {
      await invoke('add_rule', { form: form });
      const name = form.name;
      state.draft = null;
      toast(t('rule.added'), name, 'ok');
      await refresh(false);
    } catch (e) {
      toast(t('rule.add_failed'), e.message, 'error');
    }
  }

  const save = document.createElement('button');
  save.className = 'mini primary';
  save.textContent = t('common.save');
  save.addEventListener('click', saveDraft);
  wrap.appendChild(save);
  const cancel = document.createElement('button');
  cancel.className = 'mini';
  cancel.textContent = t('common.cancel');
  cancel.addEventListener('click', function () {
    state.draft = null;
    renderRows();
  });
  wrap.appendChild(cancel);
  tdAct.appendChild(wrap);
  tr.appendChild(tdAct);

  return tr;
}

// ---------------------------------------------------------------------------
// 文件夹清理（清空 / 按时间删除）
// ---------------------------------------------------------------------------

function nowSecs() { return Math.floor(Date.now() / 1000); }

/** 「昨天及更早」的界点 = 今天 00:00（本地时区）。早于它的就是昨天或更早。 */
function todayMidnightSecs() {
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return Math.floor(d.getTime() / 1000);
}

function toLocalInputValue(d) {
  const p = function (n) { return String(n).padStart(2, '0'); };
  return d.getFullYear() + '-' + p(d.getMonth() + 1) + '-' + p(d.getDate()) +
    'T' + p(d.getHours()) + ':' + p(d.getMinutes());
}

/**
 * 打开清理对话框。
 *
 * `targets` 是清理目标数组：单条规则时 `[{kind:'rule', id}]`；批量清理时
 * 把规则目标一次性传入，共用同一套策略。破坏性操作，走「先预演、看到清单再点确认删除」
 * 两步，删除按钮在预演前禁用。
 */
async function openCleanDialog(targets, headingText, dialogTitle) {
  const multi = targets.length > 1;
  const body = document.createElement('div');

  const head = document.createElement('div');
  head.className = 'field';
  head.textContent = headingText;
  body.appendChild(head);

  const warn = document.createElement('div');
  warn.className = 'hint warn';
  warn.textContent = t('clean.warn');
  body.appendChild(warn);

  // 策略单选
  const modes = [
    ['empty', t('clean.mode.empty')],
    ['before_day', t('clean.mode.day')],
    ['before_hour', t('clean.mode.hour')],
    ['between', t('clean.mode.between')],
    ['space', t('clean.mode.space')],
  ];
  const radios = [];
  modes.forEach(function (m, i) {
    const label = document.createElement('label');
    label.className = 'inline-check';
    const r = document.createElement('input');
    r.type = 'radio';
    r.name = 'cleanmode';
    r.value = m[0];
    if (i === 0) r.checked = true;
    label.appendChild(r);
    const s = document.createElement('span');
    s.textContent = m[1];
    label.appendChild(s);
    body.appendChild(label);
    radios.push(r);
  });

  // 任意小时数（仅 before_hour 时显示）：默认 1 小时，留空按 1 小时算。
  const hourWrap = document.createElement('div');
  hourWrap.className = 'field';
  const hourLabel = document.createElement('label');
  hourLabel.textContent = t('clean.hours_label');
  const hourInput = document.createElement('input');
  hourInput.type = 'number';
  hourInput.className = 'input';
  hourInput.min = '1';
  hourInput.step = '1';
  hourInput.value = '1';
  hourInput.placeholder = t('clean.hours_ph');
  hourInput.style.maxWidth = '180px';
  hourWrap.appendChild(hourLabel);
  hourWrap.appendChild(hourInput);
  body.appendChild(hourWrap);

  // 时间段输入（仅 between 时显示）
  const rangeWrap = document.createElement('div');
  rangeWrap.className = 'field';
  const row = document.createElement('div');
  row.className = 'field-row';
  const fromInput = document.createElement('input');
  fromInput.type = 'datetime-local';
  fromInput.className = 'input';
  const toInput = document.createElement('input');
  toInput.type = 'datetime-local';
  toInput.className = 'input';
  const yesterday = new Date();
  yesterday.setHours(0, 0, 0, 0);
  fromInput.value = toLocalInputValue(yesterday);
  toInput.value = toLocalInputValue(new Date());
  const fl = document.createElement('label');
  fl.textContent = t('clean.from');
  const tl = document.createElement('label');
  tl.textContent = t('clean.to');
  row.appendChild(fl); row.appendChild(fromInput);
  row.appendChild(tl); row.appendChild(toInput);
  rangeWrap.appendChild(row);
  body.appendChild(rangeWrap);

  // 水位下限输入（仅 space 时显示）：保留时间最近的若干容量，从最旧删起。
  const spaceWrap = document.createElement('div');
  spaceWrap.className = 'field';
  const spaceLabel = document.createElement('label');
  spaceLabel.textContent = t('clean.keep_floor');
  const spaceRow = document.createElement('div');
  spaceRow.className = 'field-row';
  const spaceNum = document.createElement('input');
  spaceNum.type = 'number';
  spaceNum.className = 'input';
  spaceNum.min = '1';
  spaceNum.step = '1';
  spaceNum.value = '100';
  spaceNum.placeholder = t('clean.keep_floor_ph');
  const spaceUnit = document.createElement('select');
  spaceUnit.className = 'select';
  // MB 在前：容量单位默认 MB。
  [['mb', t('rule.auto.unit_mb')], ['gb', t('rule.auto.unit_gb')]].forEach(function (u) {
    const o = document.createElement('option');
    o.value = u[0];
    o.textContent = u[1];
    spaceUnit.appendChild(o);
  });
  spaceRow.appendChild(spaceNum);
  spaceRow.appendChild(spaceUnit);
  const spaceHint = document.createElement('div');
  spaceHint.className = 'hint';
  spaceHint.textContent = t('clean.keep_floor_hint');
  spaceWrap.appendChild(spaceLabel);
  spaceWrap.appendChild(spaceRow);
  spaceWrap.appendChild(spaceHint);
  body.appendChild(spaceWrap);

  // 保留类型：这些扩展名的文件一律不删（对所有目标生效，见 fsops::exclude::Excludes）
  const keepWrap = document.createElement('div');
  keepWrap.className = 'field';
  const keepLabel = document.createElement('label');
  keepLabel.textContent = t('clean.keep_types');
  const keepInput = document.createElement('input');
  keepInput.type = 'text';
  keepInput.className = 'input';
  keepInput.placeholder = t('clean.keep_types_ph');
  const keepHint = document.createElement('div');
  keepHint.className = 'hint';
  keepHint.textContent = t('clean.keep_types_hint');
  keepWrap.appendChild(keepLabel);
  keepWrap.appendChild(keepInput);
  keepWrap.appendChild(keepHint);
  body.appendChild(keepWrap);

  function selectedMode() {
    for (const r of radios) { if (r.checked) return r.value; }
    return 'empty';
  }
  function syncRange() {
    const mode = selectedMode();
    if (mode === 'between') rangeWrap.classList.remove('hidden');
    else rangeWrap.classList.add('hidden');
    if (mode === 'space') spaceWrap.classList.remove('hidden');
    else spaceWrap.classList.add('hidden');
    if (mode === 'before_hour') hourWrap.classList.remove('hidden');
    else hourWrap.classList.add('hidden');
  }
  radios.forEach(function (r) { r.addEventListener('change', syncRange); });
  syncRange();

  function buildPolicy() {
    const mode = selectedMode();
    if (mode === 'empty') return { mode: 'empty' };
    if (mode === 'before_day') return { mode: 'before', cutoff: todayMidnightSecs() };
    if (mode === 'before_hour') {
      const h = Math.max(1, Math.floor(Number(hourInput.value)) || 1);
      return { mode: 'before', cutoff: nowSecs() - h * 3600 };
    }
    if (mode === 'space') {
      const n = Math.max(1, Math.floor(Number(spaceNum.value)) || 1);
      return { mode: 'keep_within', keep_bytes: spaceUnit.value === 'mb' ? n * 1048576 : n * 1073741824 };
    }
    const from = Math.floor(new Date(fromInput.value).getTime() / 1000);
    const to = Math.floor(new Date(toInput.value).getTime() / 1000);
    return { mode: 'between', from: from, to: to };
  }

  /** 解析「保留这些类型」输入：逗号/分号/空白分隔，逐项去空白，空项丢弃。 */
  function keepTypes() {
    return keepInput.value
      .split(/[,，;；\s]+/)
      .map(function (s) { return s.trim(); })
      .filter(function (s) { return s.length > 0; });
  }

  // 预演结果区
  const previewHost = document.createElement('div');
  previewHost.className = 'field';
  body.appendChild(previewHost);

  const btnRow = document.createElement('div');
  btnRow.className = 'field-row';
  const previewBtn = document.createElement('button');
  previewBtn.className = 'btn';
  previewBtn.textContent = t('clean.preview');
  const delBtn = document.createElement('button');
  delBtn.className = 'btn danger';
  delBtn.textContent = t('clean.confirm_del');
  delBtn.disabled = true;
  btnRow.appendChild(previewBtn);
  btnRow.appendChild(delBtn);
  body.appendChild(btnRow);

  // 改了策略或保留类型后，之前的预演结果就过期了 —— 作废它，逼用户重新预演
  // 再删，避免"看到的"和"删掉的"两套口径。
  function invalidatePreview() {
    previewHost.textContent = '';
    delBtn.disabled = true;
    delBtn.textContent = t('clean.confirm_del');
  }
  keepInput.addEventListener('input', invalidatePreview);
  spaceNum.addEventListener('input', invalidatePreview);
  spaceUnit.addEventListener('change', invalidatePreview);
  hourInput.addEventListener('input', invalidatePreview);
  radios.forEach(function (r) { r.addEventListener('change', invalidatePreview); });

  function ruleName(t) {
    if (t.kind === 'rule') {
      const r = state.snap.rules.find(function (x) { return x.id === t.id; });
      return r ? r.name : ('#' + t.id);
    }
    return t.path;
  }

  function renderPreview(pv) {
    previewHost.textContent = '';
    const sum = document.createElement('div');
    sum.className = pv.files ? 'hint' : 'hint ok';
    sum.textContent = pv.files
      ? tf('clean.will_delete', { summary: pv.summary, dir: pv.dir })
      : tf('clean.none_match', { dir: pv.dir });
    previewHost.appendChild(sum);
    if (pv.links_skipped) {
      const lk = document.createElement('div');
      lk.className = 'hint warn';
      lk.textContent = tf('clean.links_skipped', { n: pv.links_skipped });
      previewHost.appendChild(lk);
    }
    if (pv.excluded_skipped) {
      const ex = document.createElement('div');
      ex.className = 'hint warn';
      ex.textContent = tf('clean.excluded', { n: pv.excluded_skipped });
      previewHost.appendChild(ex);
    }
    (pv.sample || []).forEach(function (s) {
      const line = document.createElement('div');
      line.className = 'hint';
      line.textContent = '· ' + s.path + '　' + humanBytes(s.bytes) + '　' +
        new Date(s.mtime * 1000).toLocaleString();
      previewHost.appendChild(line);
    });
    if (pv.files > 20) {
      const more = document.createElement('div');
      more.className = 'hint';
      more.textContent = tf('clean.more', { n: pv.files });
      previewHost.appendChild(more);
    }
    delBtn.disabled = !pv.files;
    delBtn.textContent = pv.files ? tf('clean.confirm_del_n', { n: pv.files }) : t('clean.confirm_del');
  }

  function renderBatchPreview(rows, files, bytes, linksSkipped, excludedSkipped) {
    previewHost.textContent = '';
    const sum = document.createElement('div');
    sum.className = files ? 'hint' : 'hint ok';
    sum.textContent = files
      ? tf('clean.batch_will_delete', { files: files, size: humanBytes(bytes), rules: rows.length })
      : t('clean.batch_none');
    previewHost.appendChild(sum);
    if (linksSkipped) {
      const lk = document.createElement('div');
      lk.className = 'hint warn';
      lk.textContent = tf('clean.batch_links', { n: linksSkipped });
      previewHost.appendChild(lk);
    }
    if (excludedSkipped) {
      const ex = document.createElement('div');
      ex.className = 'hint warn';
      ex.textContent = tf('clean.excluded', { n: excludedSkipped });
      previewHost.appendChild(ex);
    }
    rows.forEach(function (it) {
      const line = document.createElement('div');
      line.className = it.error ? 'hint err' : 'hint';
      line.textContent = it.error
        ? tf('clean.item_error', { name: it.name, err: it.error })
        : ('· ' + it.name + '　' + (it.pv.files ? tf('clean.item_count', { files: it.pv.files, size: it.pv.bytes_text }) : t('clean.item_none')) + '　' + it.pv.dir);
      previewHost.appendChild(line);
    });
    delBtn.disabled = !files;
    delBtn.textContent = files ? tf('clean.confirm_del_n', { n: files }) : t('clean.confirm_del');
  }

  previewBtn.addEventListener('click', async function () {
    const policy = buildPolicy();
    const types = keepTypes();
    previewBtn.disabled = true;
    try {
      if (!multi) {
        const pv = await invoke('clean_preview', { target: targets[0], policy: policy, typeExcludes: types });
        renderPreview(pv);
      } else {
        const rows = [];
        let files = 0, bytes = 0, linksSkipped = 0, excludedSkipped = 0;
        for (let i = 0; i < targets.length; i++) {
          previewHost.textContent = '';
          const st = document.createElement('div');
          st.className = 'hint';
          st.textContent = tf('clean.preview_progress', { i: i + 1, n: targets.length });
          previewHost.appendChild(st);
          const name = ruleName(targets[i]);
          try {
            const pv = await invoke('clean_preview', { target: targets[i], policy: policy, typeExcludes: types });
            rows.push({ name: name, pv: pv });
            files += pv.files;
            bytes += pv.bytes;
            linksSkipped += pv.links_skipped;
            excludedSkipped += pv.excluded_skipped || 0;
          } catch (e) {
            rows.push({ name: name, error: e.message });
          }
        }
        renderBatchPreview(rows, files, bytes, linksSkipped, excludedSkipped);
      }
    } catch (e) {
      toast(t('clean.preview_failed'), e.message, 'error');
    } finally {
      previewBtn.disabled = false;
    }
  });

  delBtn.addEventListener('click', async function () {
    delBtn.disabled = true;
    const policy = buildPolicy();
    const types = keepTypes();
    try {
      if (!multi) {
        const r = await invoke('clean_run', { target: targets[0], policy: policy, dryRun: false, typeExcludes: types });
        closeModal();
        toast(t('clean.done'), r.summary, r.skipped ? 'warn' : 'ok');
      } else {
        let deletedFiles = 0, deletedBytes = 0, skipped = 0, failed = 0;
        for (let i = 0; i < targets.length; i++) {
          delBtn.textContent = tf('clean.cleaning_progress', { i: i + 1, n: targets.length });
          try {
            const r = await invoke('clean_run', { target: targets[i], policy: policy, dryRun: false, typeExcludes: types });
            deletedFiles += r.deleted_files;
            deletedBytes += r.deleted_bytes;
            skipped += r.skipped;
          } catch (e) {
            failed++;
          }
        }
        closeModal();
        toast(t('clean.batch_done'),
          tf('clean.batch_done_sub', { files: deletedFiles, size: humanBytes(deletedBytes) }) +
          (skipped ? tf('clean.batch_skipped', { n: skipped }) : '') + (failed ? tf('clean.batch_failed', { n: failed }) : ''),
          failed ? 'warn' : 'ok');
      }
      await refresh(true);
    } catch (e) {
      toast(t('clean.failed'), e.message, 'error');
      delBtn.disabled = false;
    }
  });

  openModal({
    title: dialogTitle || (multi ? t('clean.batch_title') : t('clean.title')),
    body: body,
    wide: true,
    actions: [{ label: t('common.close'), onClick: closeModal }],
  });
}

/** 底部「清理…」：按统一策略清理全部条目的目标目录。 */
async function openBatchCleanDialog() {
  const rules = state.snap.rules || [];
  if (!rules.length) {
    toast(t('clean.nothing'), t('clean.nothing_hint'), 'warn');
    return;
  }
  const targets = rules.map(function (r) { return { kind: 'rule', id: r.id }; });
  await openCleanDialog(targets, tf('clean.batch_heading', { n: rules.length }));
}

/** 工具栏「清理…」：按统一策略清理所有已选条目的目标目录。 */
async function openSelectedCleanDialog() {
  if (!requirePicked(t('pick.clean'))) return;
  await openCleanTargets(pickedIds());
}

// ---------------------------------------------------------------------------
// 磁盘空间占用统计
// ---------------------------------------------------------------------------

/** 扫描进度提示节点；扫描中由 td-disk-progress 事件更新。 */
let scanHint = null;

backend.listen('td-disk-progress', function (ev) {
  if (!scanHint) return;
  const p = ev.payload || {};
  const total = p.total || 0;
  scanHint.textContent = total
    ? tf('disk.scan_item', { name: (p.name || ''), i: (p.index + 1), n: total })
    : t('disk.scan_volumes');
});

/** 工具栏「空间统计」：实时扫描各规则缓存占用，并汇总相关卷容量。 */
async function openDiskStats() {
  const body = document.createElement('div');

  const status = document.createElement('div');
  status.className = 'hint';
  status.textContent = t('disk.scanning');
  body.appendChild(status);

  const host = document.createElement('div');
  body.appendChild(host);

  openModal({
    title: t('disk.title'),
    body: body,
    wide: true,
    actions: [
      { label: t('disk.rescan'), onClick: function () { load(); } },
      { label: t('common.close'), onClick: closeModal },
    ],
  });

  async function load() {
    host.textContent = '';
    status.className = 'hint';
    status.textContent = t('disk.scanning');
    scanHint = status;
    try {
      const data = await invoke('disk_usage');
      renderDiskStats(host, status, data);
      // 把本次扫描结果回填到主列表的「占用(M)」列（rule_id -> 条目）。
      const usage = {};
      (data.rules || []).forEach(function (r) { usage[r.id] = r; });
      state.diskUsage = usage;
      renderRows();
    } catch (e) {
      status.className = 'hint err';
      status.textContent = t('disk.failed') + '：' + e.message;
    } finally {
      scanHint = null;
    }
  }
  await load();
}

function renderDiskStats(host, status, data) {
  host.textContent = '';
  status.className = 'hint ok';
  status.textContent = tf('disk.summary', { rules: data.rule_count, size: data.total_bytes_text, ms: data.elapsed_ms });

  // ---- 各规则占用排行（横向条形图） ----
  const barsTitle = document.createElement('div');
  barsTitle.className = 'chart-title';
  barsTitle.textContent = t('disk.bars_title');
  host.appendChild(barsTitle);

  const rules = (data.rules || []).filter(function (r) { return r.bytes > 0; });
  if (!rules.length) {
    const none = document.createElement('div');
    none.className = 'hint';
    none.textContent = t('disk.bars_none');
    host.appendChild(none);
  } else {
    const max = rules.reduce(function (m, r) { return Math.max(m, r.bytes); }, 1);
    const bars = document.createElement('div');
    bars.className = 'bars';
    rules.forEach(function (r) {
      bars.appendChild(barRow(r.name, (r.bytes / max) * 100, r.bytes_text, r.dir));
    });
    host.appendChild(bars);
  }

  // ---- 磁盘卷容量 ----
  const volTitle = document.createElement('div');
  volTitle.className = 'chart-title';
  volTitle.textContent = t('disk.vol_title');
  host.appendChild(volTitle);

  const vols = data.volumes || [];
  if (!vols.length) {
    const none = document.createElement('div');
    none.className = 'hint';
    none.textContent = t('disk.vol_none');
    host.appendChild(none);
  } else {
    const list = document.createElement('div');
    list.className = 'vol-list';
    vols.forEach(function (v) { list.appendChild(volumeRow(v)); });
    host.appendChild(list);
  }
}

function barRow(name, pct, val, title) {
  const row = document.createElement('div');
  row.className = 'bar-row';
  const nm = document.createElement('div');
  nm.className = 'bar-name';
  nm.textContent = name;
  nm.title = title || name;
  const track = document.createElement('div');
  track.className = 'bar-track';
  const fill = document.createElement('div');
  fill.className = 'bar-fill';
  fill.style.width = Math.max(1, Math.min(100, pct)) + '%';
  track.appendChild(fill);
  const va = document.createElement('div');
  va.className = 'bar-val';
  va.textContent = val;
  row.appendChild(nm);
  row.appendChild(track);
  row.appendChild(va);
  return row;
}

function volumeRow(v) {
  const row = document.createElement('div');
  row.className = 'vol-row';
  const head = document.createElement('div');
  head.className = 'vol-head';
  const nm = document.createElement('div');
  nm.className = 'vol-name';
  nm.textContent = (v.roles && v.roles.length)
    ? tf('disk.vol_roles', { path: v.path, roles: v.roles.join(' · ') })
    : v.path;
  const txt = document.createElement('div');
  txt.className = 'vol-text';
  txt.textContent = tf('disk.vol_text', { used: v.used_text, total: v.total_text, free: v.free_text, pct: v.used_pct.toFixed(1) });
  head.appendChild(nm);
  head.appendChild(txt);
  row.appendChild(head);
  const track = document.createElement('div');
  track.className = 'vol-track';
  const fill = document.createElement('div');
  fill.className = 'vol-fill' + (v.used_pct >= 90 ? ' hot' : v.used_pct >= 75 ? ' warm' : '');
  fill.style.width = Math.max(0, Math.min(100, v.used_pct)) + '%';
  track.appendChild(fill);
  row.appendChild(track);
  return row;
}

// ---------------------------------------------------------------------------
// 行右键菜单：对"所选条目"执行特定操作
//
// 作用范围（与资源管理器一致）：右键点中的行若已在勾选集合里，就对**全部已
// 勾选条目**生效；否则只对右键点中的这一条生效 —— 不改动勾选状态，只用
// .ctx-target 高亮标出作用对象。模板行同理，作用对象换成内置模板。
// ---------------------------------------------------------------------------

/** 当前打开的右键菜单目标行；null 表示没开。 */
let ctxTarget = null;

/** 复制多条路径时的分隔符（换行，便于直接粘进命令行）。 */
const CTX_NL = String.fromCharCode(10);

function closeRowMenu() {
  const m = $('row-menu');
  if (m) { m.classList.add('hidden'); m.textContent = ''; }
  document.querySelectorAll('tr.ctx-target').forEach(function (tr) { tr.classList.remove('ctx-target'); });
  ctxTarget = null;
}

/** 右键点中的行在勾选集合内 -> 整批；否则只此一条。 */
function scopeIds(clickedId) { return state.picked.has(clickedId) ? pickedIds() : [clickedId]; }
function scopeTplKeys(clickedKey) { return state.pickedTpl.has(clickedKey) ? Array.from(state.pickedTpl) : [clickedKey]; }

function mkCtxHead(text) {
  const h = document.createElement('div');
  h.className = 'ctx-head';
  h.textContent = text;
  return h;
}

function mkCtxSep() {
  const s = document.createElement('div');
  s.className = 'menu-sep';
  return s;
}

/** 菜单项：点击先收起菜单再执行动作。disabled 的项保留位置但不可点。 */
function mkCtxItem(label, onClick, opts) {
  opts = opts || {};
  const b = document.createElement('button');
  b.className = 'menu-item' + (opts.danger ? ' danger' : '');
  b.textContent = label;
  if (opts.disabled) {
    b.disabled = true;
  } else {
    b.addEventListener('click', function () { closeRowMenu(); onClick(); });
  }
  return b;
}

function openRowMenu(ev, tr) {
  const m = $('row-menu');
  if (!m) return;
  closeRowMenu();
  m.textContent = '';

  const ruleId = tr.dataset.id ? Number(tr.dataset.id) : null;
  if (ruleId !== null && !Number.isNaN(ruleId)) {
    buildRuleMenu(m, scopeIds(ruleId));
  } else if (tr.dataset.tpl) {
    buildTplMenu(m, scopeTplKeys(tplKey(tr.dataset.tpl)));
  } else {
    return;
  }
  tr.classList.add('ctx-target');
  ctxTarget = tr;

  // 先摆到左上量尺寸，再按视口夹紧，避免菜单贴边溢出。
  m.classList.remove('hidden');
  m.style.left = '0px';
  m.style.top = '0px';
  const rect = m.getBoundingClientRect();
  const pad = 8;
  const maxX = Math.max(pad, window.innerWidth - rect.width - pad);
  const maxY = Math.max(pad, window.innerHeight - rect.height - pad);
  m.style.left = Math.min(Math.max(pad, ev.clientX), maxX) + 'px';
  m.style.top = Math.min(Math.max(pad, ev.clientY), maxY) + 'px';
}

function buildRuleMenu(m, ids) {
  const single = ids.length === 1;
  const rules = ids.map(function (id) {
    return state.snap.rules.find(function (r) { return r.id === id; });
  }).filter(Boolean);
  if (!rules.length) return;
  const anyLinked = rules.some(function (r) { return r.badge === '✅' || r.badge === '⚠️'; });
  const anyUnlinked = rules.some(function (r) { return r.badge === '⚠️' || r.badge === '❌'; });
  const allOn = rules.every(function (r) { return r.want_enabled; });
  const allOff = rules.every(function (r) { return !r.want_enabled; });
  const allHidden = rules.every(function (r) { return r.hidden; });
  const allShown = rules.every(function (r) { return !r.hidden; });

  m.appendChild(mkCtxHead(single ? rules[0].name : tf('ctx.title_rules', { n: rules.length })));
  m.appendChild(mkCtxItem(t('detail.act.apply'), function () { runApply(ids, false); }));
  m.appendChild(mkCtxItem(t('detail.act.restore'), function () { runRestore(ids); }, { disabled: !anyLinked }));
  m.appendChild(mkCtxItem(t('detail.act.relink'), function () { doRelink(ids[0]); }, { disabled: !single || !anyUnlinked }));
  m.appendChild(mkCtxItem(t('detail.act.locks'), function () { doLocks(ids[0]); }, { disabled: !single }));
  m.appendChild(mkCtxItem(t('detail.act.clean'), function () { openCleanTargets(ids); }));
  m.appendChild(mkCtxSep());
  m.appendChild(mkCtxItem(t('detail.act.edit'), function () { focusInlineEdit(ids[0]); }, { disabled: !single }));
  m.appendChild(mkCtxItem(t('detail.act.duplicate'), function () { duplicateRuleToDraft(rules[0]); }, { disabled: !single }));
  m.appendChild(mkCtxItem(t('detail.act.copy_source'), function () {
    copyText(rules.map(function (r) { return r.source; }).join(CTX_NL));
  }));
  m.appendChild(mkCtxItem(t('detail.act.reveal'), function () { revealSource(rules[0].source); }, { disabled: !single }));
  m.appendChild(mkCtxSep());
  m.appendChild(mkCtxItem(t('toolbar.enable'), function () { setRulesEnabled(ids, true); }, { disabled: allOn }));
  m.appendChild(mkCtxItem(t('toolbar.disable'), function () { setRulesEnabled(ids, false); }, { disabled: allOff }));
  m.appendChild(mkCtxItem(t('ctx.hide'), function () { setRulesHidden(ids, true); }, { disabled: allHidden }));
  m.appendChild(mkCtxItem(t('ctx.unhide'), function () { setRulesHidden(ids, false); }, { disabled: allShown }));
  m.appendChild(mkCtxItem(t('toolbar.delete'), function () { confirmDeleteRules(ids); }, { danger: true }));
}

function buildTplMenu(m, keys) {
  const tpls = (state.presets || []).filter(function (p) { return keys.indexOf(tplKey(p.source_template)) >= 0; });
  if (!tpls.length) return;
  m.appendChild(mkCtxHead(tpls.length > 1 ? tf('ctx.title_tpl_multi', { n: tpls.length }) : tf('ctx.title_tpl', { name: tpls[0].name })));
  m.appendChild(mkCtxItem(t('tpl.add_apply'), function () { applyTemplates(tpls); }));
  m.appendChild(mkCtxSep());
  m.appendChild(mkCtxItem(t('detail.act.copy_source'), function () {
    copyText(tpls.map(function (p) { return p.source; }).join(CTX_NL));
  }));
  m.appendChild(mkCtxItem(t('detail.act.reveal'), function () { revealSource(tpls[0].source); }, { disabled: tpls.length > 1 }));
}

/** 打开源目录（行右键菜单 / 详情面板共用同一后端命令）。 */
function revealSource(path) {
  invoke('reveal_in_explorer', { path: path }).catch(function (e) {
    toast(t('toast.reveal_failed'), e.message, 'error');
  });
}

/** 列表行右键 -> 上下文菜单。只在规则行 / 模板行上生效；列表区域内其它
   位置右键只收起菜单，不弹系统菜单。 */
document.addEventListener('contextmenu', function (ev) {
  const row = ev.target.closest ? ev.target.closest('tr[data-id], tr[data-tpl]') : null;
  if (row) { ev.preventDefault(); openRowMenu(ev, row); return; }
  if (ev.target.closest && ev.target.closest('main.table-wrap')) {
    ev.preventDefault();
    closeRowMenu();
    return;
  }
  closeRowMenu();
});
document.addEventListener('scroll', function () { if (ctxTarget) closeRowMenu(); }, true);
window.addEventListener('resize', function () { if (ctxTarget) closeRowMenu(); });

// ---------------------------------------------------------------------------
// 事件绑定（事件委托）
// ---------------------------------------------------------------------------

document.addEventListener('click', async function (ev) {
  const target = ev.target.closest('[data-act]');
  // 行右键菜单：点菜单以外任意位置都收起（菜单项自身的 click 会先收起再执行动作）。
  if (ctxTarget && !(ev.target.closest && ev.target.closest('#row-menu'))) closeRowMenu();
  if (target) {
    const act = target.dataset.act;
    const id = Number(target.dataset.id);

    if (act === 'toggle-flag') {
      const r = (state.snap.rules || []).find(function (x) { return x.id === id; });
      await toggleRuleFlag(r, target.dataset.flag);
      return;
    }
    if (act === 'apply') { await runApply([id], false); return; }

    if (act === 'restore') { await runRestore([id]); return; }
    if (act === 'relink') { await doRelink(id); return; }
    if (act === 'locks') { await doLocks(id); return; }
    if (act === 'clean') {
      const r = state.snap.rules.find(function (x) { return x.id === id; });
      await openCleanDialog([{ kind: 'rule', id: id }], tf('clean.title_rule', { name: r ? r.name : ('#' + id) }));
      return;
    }
    if (act === 'edit') { focusInlineEdit(id); return; }
    if (act === 'copy-source') {
      const r = state.snap.rules.find(function (x) { return x.id === id; });
      if (r) await copyText(r.source);
      return;
    }
    if (act === 'reveal') {
      const r = state.snap.rules.find(function (x) { return x.id === id; });
      if (!r) return;
      try { await invoke('reveal_in_explorer', { path: r.source }); }
      catch (e) { toast(t('toast.reveal_failed'), e.message, 'error'); }
      return;
    }
    // 内置模板行：点名称把模板载入顶部草稿行，先改再存；「添加并应用」仍是一步到位。
    if (act === 'tpl-draft') {
      if (inlineSaving) return;
      const p = (state.presets || []).find(function (x) { return tplKey(x.source_template) === tplKey(target.dataset.tpl); });
      if (!p) return;
      if (activeCellEditor) {
        // 当前格还在编辑：先提交，提交完再载入模板到草稿行。
        pendingTemplate = p;
        activeCellEditor.commit();
        return;
      }
      startDraftRule(p);
      return;
    }
    if (act === 'tpl-apply') {
      const p = (state.presets || []).find(function (x) { return tplKey(x.source_template) === tplKey(target.dataset.tpl); });
      if (!p) return;
      await applyTemplates([p]);
      return;
    }
    return;
  }

  // 单击可编辑单元格：名称 / 分类 / 链接 / 源 / 目标 / 自动清理 / 上限 / 排除 / 备注。
  // 先切到该条规则，再由 beginCellEdit 原地换成输入控件（不弹任何对话框）。
  const cell = ev.target.closest ? ev.target.closest('td[data-field]') : null;
  if (cell && !(ev.target.closest && ev.target.closest('.cell-editor'))) {
    if (cell.classList.contains('editing') || inlineSaving) return;
    const id = Number(cell.dataset.id);
    const rule = (state.snap.rules || []).find(function (r) { return r.id === id; });
    if (rule) {
      if (activeCellEditor) {
        // 当前格还在编辑：先提交它，成功/取消后自动落到刚点的这一格。
        pendingCell = { id: id, field: cell.dataset.field };
        activeCellEditor.commit();
        return;
      }
      state.focusId = id;
      document.querySelectorAll('#rows tr.selected').forEach(function (tr) { tr.classList.remove('selected'); });
      const tr = cell.closest('tr');
      if (tr) tr.classList.add('selected');
      if (!state.treeOpen.has(rule.category)) state.treeOpen.add(rule.category);
      renderSidebar();

      beginCellEdit(cell, rule, cell.dataset.field);
    }
    return;
  }

  const el = ev.target;
  const closest = el.closest ? el.closest.bind(el) : function () { return null; };
  // 表头点击排序（th[data-sort]）：同列切换升降序，新列默认升序。
  const sortTh = closest('th[data-sort]');
  if (sortTh) {
    const key = sortTh.dataset.sort;
    if (state.sort.key === key) {
      state.sort.dir = state.sort.dir === 'asc' ? 'desc' : 'asc';
    } else {
      state.sort = { key: key, dir: 'asc' };
    }
    renderRows();
    renderSortIndicators();
    return;
  }
  // 「更多」菜单：点菜单外任意处收起；点「更多」切换；点菜单项后收起再走下面的分支。
  if (!closest('.more-wrap')) hideMoreMenu();
  if (el.id === 'btn-more') { toggleMoreMenu(); return; }
  // 列显示开关要能连续勾选，点它时不要收起菜单；其它菜单项照旧收起。
  if (closest('#more-menu') && !closest('.menu-check')) hideMoreMenu();
  if (el.id === 'btn-refresh') await refresh(true);
  else if (el.id === 'btn-add') startDraftRule();
  else if (el.id === 'btn-apply') { if (requireAnyPick(t('pick.apply'))) await applyPickedAll(); }
  else if (el.id === 'btn-restore') { if (requirePicked(t('pick.restore'))) await runRestore(pickedIds()); }
  else if (el.id === 'btn-categorize') {
    if (!requirePicked(t('pick.categorize'))) return;
    openCategorizeDialog();
  } else if (el.id === 'btn-enable' || el.id === 'btn-disable') {
    if (!requirePicked(t('pick.toggle'))) return;
    await setRulesEnabled(pickedIds(), el.id === 'btn-enable');
  } else if (el.id === 'btn-locks') {
    if (!requirePicked(t('pick.detect'))) return;
    await doLocks(pickedIds()[0]);
  } else if (el.id === 'btn-clean') {
    await openSelectedCleanDialog();
  } else if (el.id === 'btn-disk') {
    await openDiskStats();
  } else if (el.id === 'btn-relink') {
    if (!requirePicked(t('pick.relink'))) return;
    for (const id of pickedIds()) await doRelink(id);
  } else if (el.id === 'btn-delete') {
    if (!requirePicked(t('pick.delete'))) return;
    confirmDeleteRules(pickedIds());
  } else if (el.id === 'btn-undo') {
    await undoLast();
  } else if (el.id === 'btn-boot') await doBoot();
  else if (el.id === 'btn-restore-all') await restoreAllRules();
  else if (el.id === 'btn-ops') await openOps();
  else if (el.id === 'btn-settings') await openSettings();
  else if (el.id === 'modal-close') closeModal();
});

/** 切换工具栏「更多」下拉（诊断 / 低频操作收纳在这里，主按钮区只留高频动作）。 */
function toggleMoreMenu() {
  const m = document.getElementById('more-menu');
  if (m) m.classList.toggle('hidden');
}

function hideMoreMenu() {
  const m = document.getElementById('more-menu');
  if (m) m.classList.add('hidden');
}

document.addEventListener('change', function (ev) {
  const el = ev.target;
  if (el.dataset && el.dataset.act === 'pick') {
    const id = Number(el.dataset.id);
    if (el.checked) state.picked.add(id); else state.picked.delete(id);
    renderRows();
    return;
  }
  // 内置模板行的勾选：模板还没有 id，用 source_template 小写当键。
  if (el.dataset && el.dataset.act === 'pick-tpl') {
    const key = tplKey(el.dataset.tpl);
    if (el.checked) state.pickedTpl.add(key); else state.pickedTpl.delete(key);
    renderRows();
    return;
  }
  // 「更多」菜单里的列显示开关：只改前端展示，存 localStorage。
  if (el.dataset && el.dataset.col && el.type === 'checkbox') {
    const key = el.dataset.col;
    if (COL_KEYS.indexOf(key) < 0) return;
    if (el.checked) state.hiddenCols.delete(key); else state.hiddenCols.add(key);
    saveHiddenCols();
    applyColumnVisibility();
    return;
  }
  if (el.id === 'filter-category') { applyCategory(el.value); }
  else if (el.id === 'only-enabled') { state.filters.onlyEnabled = el.checked; renderRows(); }
  else if (el.id === 'only-attention') { state.filters.onlyAttention = el.checked; renderRows(); }
  else if (el.id === 'show-hidden') { state.filters.showHidden = el.checked; renderSidebar(); renderRows(); }
  else if (el.id === 'sel-all' || el.id === 'sel-all-head') {
    const rows = filteredRules();
    if (el.checked) rows.forEach(function (r) { state.picked.add(r.id); });
    else rows.forEach(function (r) { state.picked.delete(r.id); });
    renderRows();
  }
});

document.addEventListener('input', function (ev) {
  if (ev.target.id === 'search') {
    state.filters.q = ev.target.value;
    renderRows();
  }
});

document.addEventListener('keydown', function (ev) {
  // 保存后焦点留在单元格上：Enter / F2 直接再次进入编辑，方便键盘连续修改。
  const editCell = ev.target && ev.target.closest ? ev.target.closest('td[data-field]') : null;
  if (editCell && !(ev.target.closest && ev.target.closest('.cell-editor')) &&
      (ev.key === 'Enter' || ev.key === 'F2')) {
    ev.preventDefault();
    if (inlineSaving) return;
    const id = Number(editCell.dataset.id);
    const rule = (state.snap.rules || []).find(function (r) { return r.id === id; });
    if (rule) beginCellEdit(editCell, rule, editCell.dataset.field);
    return;
  }
  if (ev.key === 'Escape') {
    // 行右键菜单最轻，最先关（再按一次才轮到弹窗）。
    if (ctxTarget) { closeRowMenu(); return; }
    if (!$('modal').classList.contains('hidden')) {
      closeModal();
    } else if (state.focusId !== null) {
      // Esc 取消当前行 / 树节点的高亮（详情面板已移除，高亮本身仍用于定位）。
      state.focusId = null;
      renderSidebar();
      renderRows();
    }
  }
  if ((ev.ctrlKey || ev.metaKey) && ev.key === 'r') {
    ev.preventDefault();
    refresh(true);
  }
});

// ---------------------------------------------------------------------------
// 拖拽排序（同一分类内）。列表按 category 分组展示，跨分类拖拽没有意义
// （后端 ORDER BY category 优先），在 dragover 阶段就拒绝。
// 表格拖把手、侧栏树里直接拖规则节点，两种入口共用同一套落点与写库逻辑。
// ---------------------------------------------------------------------------

/** 表格行只有抓着把手才临时开启 draggable；树里的规则节点整行可拖。 */
document.addEventListener('mousedown', function (ev) {
  const h = ev.target.closest && ev.target.closest('.drag-handle');
  if (h) {
    const tr = h.closest('tr');
    if (tr) tr.draggable = true;
    return;
  }
  const node = ev.target.closest && ev.target.closest('.tree-row[data-act="rule"]');
  if (node) node.draggable = true;
});

function ruleById(id) {
  return (state.snap.rules || []).find(function (r) { return r.id === Number(id); });
}

function treeDragTarget(ev) {
  return ev.target.closest ? ev.target.closest('.tree-row[data-act="rule"]') : null;
}

document.addEventListener('dragstart', function (ev) {
  const node = treeDragTarget(ev);
  if (node) {
    const r = ruleById(node.dataset.id);
    if (!r) return;
    state.drag = { id: r.id, category: r.category, fromTree: true };
    ev.dataTransfer.effectAllowed = 'move';
    ev.dataTransfer.setData('text/plain', String(r.id));
    node.classList.add('dragging');
    return;
  }
  const tr = ev.target.closest && ev.target.closest('tr');
  if (!tr || !tr.draggable || !tr.dataset.id) return;
  const r = ruleById(tr.dataset.id);
  if (!r) return;
  state.drag = { id: r.id, category: r.category, fromTree: false };
  ev.dataTransfer.effectAllowed = 'move';
  ev.dataTransfer.setData('text/plain', String(r.id));
});

document.addEventListener('dragover', function (ev) {
  if (!state.drag) return;
  // 树内拖拽：只认同分类的规则节点，落点在上半 = 插到它前面，下半 = 后面。
  if (state.drag.fromTree) {
    const node = treeDragTarget(ev);
    if (!node) return;
    const r = ruleById(node.dataset.id);
    if (!r || r.category !== state.drag.category || r.id === state.drag.id) return;
    ev.preventDefault();
    ev.dataTransfer.dropEffect = 'move';
    clearTreeDropHints();
    const rect = node.getBoundingClientRect();
    node.classList.add(ev.clientY < rect.top + rect.height / 2 ? 'drop-before' : 'drop-after');
    return;
  }
  const tr = ev.target.closest && ev.target.closest('tr');
  if (!tr || !tr.dataset.id) return;
  const r = ruleById(tr.dataset.id);
  // 跨分类 / 拖到自己：不 preventDefault = 不允许落点
  if (!r || r.category !== state.drag.category || r.id === state.drag.id) return;
  ev.preventDefault();
  ev.dataTransfer.dropEffect = 'move';
  tr.classList.add('drag-over');
});

document.addEventListener('drop', async function (ev) {
  if (!state.drag) return;
  const drag = state.drag;
  state.drag = null;
  clearDragFeedback();

  if (drag.fromTree) {
    const node = treeDragTarget(ev);
    if (!node || !node.dataset.id) return;
    ev.preventDefault();
    const target = ruleById(node.dataset.id);
    if (!target || target.category !== drag.category || target.id === drag.id) return;
    const rect = node.getBoundingClientRect();
    await persistReorder(drag.id, target.id, ev.clientY < rect.top + rect.height / 2);
    return;
  }

  const tr = ev.target.closest && ev.target.closest('tr');
  if (!tr || !tr.dataset.id) return;
  ev.preventDefault();
  const targetId = Number(tr.dataset.id);
  if (targetId === drag.id) return;
  // 落点在行的上半 → 插到目标之前；下半 → 之后。
  const rect = tr.getBoundingClientRect();
  await persistReorder(drag.id, targetId, ev.clientY < rect.top + rect.height / 2);
});

/** 树与表格共用的写库入口：拖完即保存该分类的完整顺序。 */
async function persistReorder(dragId, targetId, before) {
  const orderedIds = categoryOrderAfterDrop(dragId, targetId, before);
  try {
    await invoke('reorder_rules', { orderedIds: orderedIds });
    await refresh(true);
  } catch (e) {
    toast(t('toast.reorder_failed'), e.message, 'error');
  }
}

document.addEventListener('dragend', function () {
  state.drag = null;
  clearDragFeedback();
});

function clearTreeDropHints() {
  document.querySelectorAll('.tree-row.drop-before, .tree-row.drop-after').forEach(function (n) {
    n.classList.remove('drop-before', 'drop-after');
  });
}

function clearDragFeedback() {
  document.querySelectorAll('tr.drag-over').forEach(function (tr) { tr.classList.remove('drag-over'); });
  document.querySelectorAll('tr[draggable="true"]').forEach(function (tr) { tr.draggable = false; });
  document.querySelectorAll('.tree-row.dragging').forEach(function (n) { n.classList.remove('dragging'); });
  document.querySelectorAll('.tree-row[draggable="true"]').forEach(function (n) { n.draggable = false; });
  clearTreeDropHints();
}

/** 组装拖拽后该分类的全量顺序（后端要求完整覆盖该分类 ——
 *  界面正被筛选/搜索时也要从全量快照取，否则被隐藏的规则顺序会悬空）。 */
function categoryOrderAfterDrop(dragId, targetId, before) {
  const all = state.snap.rules;
  const dragRule = all.find(function (r) { return r.id === dragId; });
  const group = all.filter(function (r) { return r.category === dragRule.category; });
  const rest = group.filter(function (r) { return r.id !== dragId; });
  const idx = rest.findIndex(function (r) { return r.id === targetId; });
  rest.splice(before ? idx : idx + 1, 0, dragRule);
  return rest.map(function (r) { return r.id; });
}

// 后端进度事件
backend.listen('td-progress', function (ev) {
  const p = ev.payload || {};
  const pct = p.total ? ((p.index + 0.5) / p.total) * 100 : 0;
  setProgress(pct, '#' + (p.index + 1) + '/' + p.total + ' ' + p.name + ' · ' + (p.detail || p.phase));
});

backend.listen('td-tray-boot', function (ev) {
  const p = ev.payload || {};
  toast(t('toast.tray_boot'), p.message, p.ok ? 'ok' : 'warn');
  refresh(true);
});


// ---------------------------------------------------------------------------
// Mock 后端（仅用于本地静态预览 / 无头截图核验，不参与打包逻辑）
// ---------------------------------------------------------------------------

function createMockBackend() {
  const rules = [
    mockRule(1, t('mock.rule.chrome'), 'browser', '%LOCALAPPDATA%\\Google\\Chrome\\User Data\\Default\\Cache', 'browsers\\chrome-cache', '✅', 'link_ok', { last_files: 34521, last_bytes: 1288490188, auto_clean_policy: 'older:86400', auto_clean_limit_bytes: 2147483648 }),
    mockRule(2, t('mock.rule.edge'), 'browser', '%LOCALAPPDATA%\\Microsoft\\Edge\\User Data\\Default\\Cache', 'browsers\\edge-cache', '✅', 'link_ok', { last_files: 21002, last_bytes: 734003200 }),
    mockRule(3, t('mock.rule.firefox'), 'browser', '%APPDATA%\\Mozilla\\Firefox\\Profiles\\xxxx.default-release\\cache2', 'browsers\\firefox-cache2', '⚠️', 'link_ok_empty', {}),
    mockRule(4, t('mock.rule.photoshop'), 'design', 'C:\\Users\\me\\AppData\\Roaming\\Adobe\\Common\\Media Cache Files', 'design\\ps-media-cache', '⚠️', 'link_broken', { needs_attention: true }),
    mockRule(5, t('mock.rule.vscode'), 'dev', '%APPDATA%\\Code\\Cache', 'dev\\vscode-cache', '❌', 'plain_dir', { excludes: ['logs', 'CachedData\\old'] }),
    mockRule(6, t('mock.rule.wechat'), 'office', '%APPDATA%\\Tencent\\WeChat\\FileStorage', 'office\\wechat-files', '❌', 'plain_dir', { want_enabled: false }),
    mockRule(7, t('mock.rule.npm'), 'dev', '%LOCALAPPDATA%\\npm-cache', 'dev\\npm-cache', '✅', 'link_ok', { last_files: 88213, last_bytes: 3221225472 }),
    mockRule(8, t('mock.rule.premiere'), 'design', 'C:\\Users\\me\\AppData\\Roaming\\Adobe\\Common\\Media Cache', 'design\\pr-media-cache', '❌', 'plain_dir', { needs_elevation: true, has_acl: true, want_enabled: false }),
    mockRule(9, t('mock.rule.user_temp'), 'system', '%LOCALAPPDATA%\\Temp', 'system\\user-temp', '✅', 'setting_ok', { redirect_mode: 'setting', env_vars: ['TEMP', 'TMP'], env_scope: 'user', setting_options: [{ scope: 'user', vars: ['TEMP', 'TMP'] }], last_files: 1024, last_bytes: 52428800 }),
  ];

  // 与真后端一致：内置分类在前，归类时新建的分类追加到后面。
  const categories = ['browser', 'design', 'dev', 'system', 'media', 'office', 'game', 'other'];

  // 高权限登录任务的登记态：可切换，方便在预览里走通启用 / 停用两个分支。
  // `&needs_update=1` 演示「已注册任务参数是旧版」的提示。
  const autostart = { elevated: false, startup: false, needsUpdate: PARAMS.has('needs_update') };

  /** 状态说明按当前语言生成（键在 i18n.js 的 mock.state.*，措辞与后端对齐）。
   *  切换语言时由 relabelRules() 重算，行为与真机一致（名称是用户数据，不重算）。 */
  function stateLabel(r) {
    if (r._state === 'link_ok') return t('mock.state.link_ok');
    if (r._state === 'link_ok_empty') return tf('mock.state.link_ok_empty', { target: r.target });
    if (r._state === 'link_broken') return tf('mock.state.link_broken', { target: r.target });
    if (r._state === 'setting_ok') return tf('mock.state.setting_ok', { target: r.target });
    if (r._state === 'setting_ok_empty') return tf('mock.state.setting_ok_empty', { target: r.target });
    if (r._state === 'setting_mismatch') return t('mock.state.setting_mismatch');
    return t('mock.state.plain_dir');
  }

  /** 语言切换后重算状态文案（mock.set_locale 里调用）。 */
  function relabelRules() {
    rules.forEach(function (r) {
      r.label = stateLabel(r);
      r.warnings = r.needs_elevation ? [t('rule.src.elev')] : [];
    });
  }

  function mockRule(id, name, category, tpl, sub, badge, stateKey, extra) {
    const e = extra || {};
    const r = {
      id: id, name: name, category: category,
      source_template: tpl,
      source: tpl.replace('%LOCALAPPDATA%', 'C:\\Users\\me\\AppData\\Local').replace('%APPDATA%', 'C:\\Users\\me\\AppData\\Roaming'),
      target: 'R:\\' + sub,
      target_root_id: 1, target_subpath: sub, link_type: 'junction',
      redirect_mode: e.redirect_mode || 'link',
      env_vars: e.env_vars || [],
      env_scope: e.env_scope || 'user',
      // 「系统设置」可用的组合（作用域 + 变量名）。空 = 这条规则用不了这个模式，
      // 前端据此不给出「系统设置」选项。
      setting_options: e.setting_options || [],
      want_enabled: e.want_enabled !== false, rebuild_on_boot: true,
      has_acl: !!e.has_acl, needs_elevation: !!e.needs_elevation, notes: null,
      excludes: e.excludes || [],
      auto_clean_policy: e.auto_clean_policy || '',
      auto_clean_limit_bytes: e.auto_clean_limit_bytes || 0,
      hidden: !!e.hidden,
      badge: badge, label: '',
      needs_attention: !!e.needs_attention || badge === '❌',
      unknown_vars: [], last_files: e.last_files || null, last_bytes: e.last_bytes || null,
      applied_at: e.last_files ? '2026-10-14 09:45:56' : null,
      _state: stateKey,
    };
    r.label = stateLabel(r);
    r.warnings = e.needs_elevation ? [t('rule.src.elev')] : [];
    return r;
  }

  let mockCloseToTray = true;
  let mockAllowMerge = false;
  let mockSched = { enabled: false, time: '03:00' };

  // 撤销栈（mock）：与真机同语义 —— 一条栈，一次操作 = 一条栈顶记录。
  // 删除是把整条规则搬进栈里；修改是把「改前的值」记进栈里。
  // 末尾那条就是"最近一次操作"，撤销时整批弹掉。
  let mockUndo = [];

  /** 记一条修改（字段级前后值），只记真正变了的字段。 */
  function mockPushUpdate(changes) {
    const list = changes.filter(function (c) { return c.before !== c.after; });
    if (!list.length) return;
    mockUndo.push({ kind: 'update', changes: list });
  }

  /** 两条规则同名时按 id 取第一条，构造「改前/改后」用。 */
  function mockDiff(before, after) {
    const fields = ['name', 'category', 'source_template', 'target_subpath', 'link_type',
      'redirect_mode', 'env_scope', 'want_enabled', 'rebuild_on_boot', 'notes'];
    const out = [];
    fields.forEach(function (f) {
      const b = before[f];
      const a = after[f];
      const same = Array.isArray(a) ? JSON.stringify(a) === JSON.stringify(b) : a === b;
      if (!same) out.push({ field: f, before: b, after: a });
    });
    return out;
  }

  function mockUndoView() {
    if (!mockUndo.length) return { pending: 0, kind: '', names: [] };
    const top = mockUndo[mockUndo.length - 1];
    const names = top.kind === 'delete'
      ? top.rules.map(function (r) { return r.name; })
      : top.changes.map(function (c) { return c.name; });
    return { pending: names.length, kind: top.kind, names: names };
  }

  function stats() {
    const s = { total: rules.length, enabled: 0, ok: 0, warn: 0, plain: 0, other: 0, needs_attention: 0 };
    rules.forEach(function (r) {
      if (r.want_enabled) s.enabled++;
      if (r.needs_attention) s.needs_attention++;
      if (r.badge === '✅') s.ok++;
      else if (r.badge === '⚠️') s.warn++;
      else if (r.badge === '❌') s.plain++;
      else s.other++;
    });
    return s;
  }

  function snapshot() {
    return {
      version: '0.1.0', schema_version: 1,
      data_dir: 'C:\\ProgramData\\SubLink',
      db_path: 'C:\\ProgramData\\SubLink\\sublink.db',
      categories: categories.slice(),
      roots: [{
        id: 1, label: 'RAMDisk 4G', path: 'R:', is_ramdisk: true, wait_timeout_ms: 90000,
        state: 'ready', message: tf('mock.root_ready', { free: '3.7 GB' }), unknown_vars: [],
      }],
      rules: rules.map(function (r) { const c = Object.assign({}, r); delete c._state; return c; }),
      stats: stats(),
      banners: [
        { level: 'warn', text: tf('mock.banner.attention', { n: stats().needs_attention }) },
        { level: 'error', text: tf('mock.banner.disk_critical', { label: 'RAMDisk 4G', path: 'R:', pct: '96.4', free: '147.5 MB' }), action: 'sweep' },
      ],
      open_ops: [],
      undo: mockUndoView(),
    };
  }

  const find = function (id) { return rules.find(function (r) { return r.id === id; }); };

  /** 校验前端发来的表单形状。
   * 为什么 mock 要做这件事：字段名对不上时真机是"保存失败：missing field xxx"，
   * 而 mock 若不校验，就会一路绿灯把问题藏到用户手上（这正是一次真实教训）。 */
  function assertFormShape(f) {
    const need = ['name', 'category', 'source_template', 'target_root_id',
      'target_subpath', 'link_type', 'redirect_mode', 'env_vars', 'env_scope',
      'want_enabled', 'rebuild_on_boot', 'auto_clean_limit_bytes'];
    const miss = need.filter(function (k) { return !(f && (k in f)); });
    if (miss.length) {
      throw new Error('mock 校验失败：表单缺少字段 ' + miss.join('、') +
        '（前端发出去的字段名与 Rust 结构体对不上了）');
    }
    return f;
  }

  return {
    invoke: function (name, args) {
      args = args || {};
      switch (name) {
        case 'ui_snapshot': return Promise.resolve(snapshot());
        case 'recent_events':
          return Promise.resolve([
            { id: 5, rule_id: 2, level: 'info', kind: 'applied', message: 'Edge 缓存：跨卷复制：21002 个文件 / 700.0 MB', at: '2026-10-14 09:45:56' },
            { id: 4, rule_id: 1, level: 'info', kind: 'applied', message: 'Chrome 缓存：同卷重命名：34521 个文件 / 1.2 GB', at: '2026-10-14 09:45:55' },
            { id: 3, rule_id: 4, level: 'error', kind: 'apply_failed', message: 'Photoshop 暂存盘：目标根不可用（RAM 盘未挂载）', at: '2026-10-14 09:45:12' },
            { id: 2, rule_id: null, level: 'warn', kind: 'blocked', message: '微信文件缓存：3 个文件被占用，占用者：WeChat.exe（PID 8024）', at: '2026-10-14 09:44:01' },
            { id: 1, rule_id: null, level: 'info', kind: 'boot_rebuild', message: '目标卷 1/1 就绪；规则 6 条：新建目录 6，链接自动恢复 5，本就正常 1', at: '2026-10-14 09:40:00' },
          ]);
        case 'recent_ops':
          return Promise.resolve([
            { id: 3, rule_id: 5, kind: 'apply', phase: 'failed', source_path: 'C:\\Users\\me\\AppData\\Roaming\\Code\\Cache', target_path: 'R:\\dev\\vscode-cache', file_count: 120, bytes: 4096, error: '源目录正被占用，已中止', started_at: '2026-10-14 09:45:12', finished_at: '2026-10-14 09:45:12' },
            { id: 2, rule_id: 2, kind: 'apply', phase: 'done', source_path: 'C:\\Users\\me\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\Cache', target_path: 'R:\\browsers\\edge-cache', file_count: 21002, bytes: 734003200, error: null, started_at: '2026-10-14 09:45:20', finished_at: '2026-10-14 09:45:56' },
          ]);
        case 'savings': {
          // 「当前重定向」与后端口径一致地实时算：期望开启 且 链接正常。
          // moved / cleaned 是历史累计（不在规则快照里），给一组与
          // recent_events / recent_ops 的 mock 流水对得上的数字。
          let rr = 0, rf = 0, rb = 0;
          rules.forEach(function (r) {
            if (r.want_enabled && (r._state === 'link_ok' || r._state === 'link_ok_empty')) {
              rr++; rf += r.last_files || 0; rb += r.last_bytes || 0;
            }
          });
          return Promise.resolve({
            redirected_rules: rr, redirected_files: rf, redirected_bytes: rb,
            redirected_text: humanBytes(rb),
            moved_files: 55523, moved_bytes: 2022493388, moved_text: humanBytes(2022493388),
            cleaned_files: 12830, cleaned_bytes: 1879048192, cleaned_text: humanBytes(1879048192),
          });
        }
        case 'add_root':
          return Promise.resolve(1);
        case 'delete_root':
          return Promise.resolve(null);
        case 'reorder_rules': {
          // 这些 id 原本占据的索引集合不变，但集合内按前端传来的新顺序回填。
          // 注意必须用 orderedIds 的顺序取规则 —— 直接 filter 原数组会得到旧顺序，
          // 预览里拖拽就像"没生效"。
          const set = {};
          args.orderedIds.forEach(function (id) { set[id] = true; });
          const moved = args.orderedIds.map(function (id) {
            return rules.find(function (r) { return r.id === id; });
          }).filter(Boolean);
          let mi = 0;
          const changes = [];
          const next = rules.map(function (r) {
            if (!set[r.id]) return r;
            const before = r.sort_order;
            const target = moved[mi++];
            if (target && target.sort_order !== before) {
              changes.push({
                field: 'sort_order', before: before, after: target.sort_order,
                name: target.name, id: target.id,
              });
            }
            return target;
          });
          rules.length = 0;
          rules.push.apply(rules, next);
          mockPushUpdate(changes);
          return Promise.resolve(args.orderedIds.length);
        }
        case 'add_rule': {
          const f = assertFormShape(args.form);
          const id = Math.max.apply(null, rules.map(function (r) { return r.id; })) + 1;
          rules.push(mockRule(id, f.name, f.category, f.source_template, f.target_subpath, '❌', 'plain_dir', { want_enabled: f.want_enabled, auto_clean_policy: f.auto_clean_policy || '', auto_clean_limit_bytes: f.auto_clean_limit_bytes || 0 }));
          const added = find(id);
          if (added) {
            added.link_type = f.link_type;
            added.redirect_mode = f.redirect_mode || 'link';
            added.env_vars = f.env_vars || [];
            added.env_scope = f.env_scope || 'user';
            added.notes = f.notes || null;
            added.excludes = f.excludes || [];
          }
          return Promise.resolve(id);
        }
        case 'update_rule': {
          const r = find(args.id);
          assertFormShape(args.form);
          if (r) {
            const before = {
              name: r.name, category: r.category, source_template: r.source_template,
              target_subpath: r.target_subpath, link_type: r.link_type,
              redirect_mode: r.redirect_mode, env_scope: r.env_scope,
              want_enabled: r.want_enabled, rebuild_on_boot: r.rebuild_on_boot,
              notes: r.notes,
            };
            r.name = args.form.name; r.category = args.form.category;
            r.source_template = args.form.source_template; r.target_subpath = args.form.target_subpath;
            r.source = args.form.source_template; r.target = 'R:\\' + args.form.target_subpath;
            r.target_root_id = args.form.target_root_id;
            r.link_type = args.form.link_type;
            r.redirect_mode = args.form.redirect_mode || 'link';
            r.env_vars = args.form.env_vars || [];
            r.env_scope = args.form.env_scope || 'user';
            r.want_enabled = args.form.want_enabled; r.rebuild_on_boot = args.form.rebuild_on_boot;
            r.notes = args.form.notes;
            r.excludes = args.form.excludes || [];
            r.auto_clean_policy = args.form.auto_clean_policy || '';
            r.auto_clean_limit_bytes = args.form.auto_clean_limit_bytes || 0;
            // 改了字段就进撤销栈（真机是字段级前后值，mock 保持同样语义）。
            const changes = mockDiff(before, {
              name: r.name, category: r.category, source_template: r.source_template,
              target_subpath: r.target_subpath, link_type: r.link_type,
              redirect_mode: r.redirect_mode, env_scope: r.env_scope,
              want_enabled: r.want_enabled, rebuild_on_boot: r.rebuild_on_boot,
              notes: r.notes,
            });
            changes.forEach(function (c) { c.name = r.name; c.id = r.id; });
            mockPushUpdate(changes);
          }
          return Promise.resolve(null);
        }
        case 'delete_rules': {
          const moved = [];
          args.ids.forEach(function (id) {
            const i = rules.findIndex(function (r) { return r.id === id; });
            if (i >= 0) moved.push(rules.splice(i, 1)[0]);
          });
          if (moved.length) mockUndo.push({ kind: 'delete', rules: moved });
          return Promise.resolve(args.ids.length);
        }
        case 'undo_last': {
          if (!mockUndo.length) return Promise.resolve({ restored: [], skipped: [], kind: '' });
          const top = mockUndo.pop();
          if (top.kind === 'delete') {
            // 撤销最近一次删除：把整批搬回规则列表。
            top.rules.forEach(function (r) { rules.push(r); });
            return Promise.resolve({
              restored: top.rules.map(function (r) { return r.name; }),
              skipped: [],
              kind: 'delete',
            });
          }
          // 撤销最近一次修改：把字段改回"改前值"。
          const restored = [];
          top.changes.forEach(function (c) {
            const r = find(c.id);
            if (!r) return;
            r[c.field] = c.before;
            if (!restored.length) restored.push(c.name);
          });
          return Promise.resolve({ restored: restored, skipped: [], kind: 'update' });
        }
        case 'set_rules_enabled': {
          const changes = [];
          args.ids.forEach(function (id) {
            const r = find(id);
            if (!r || r.want_enabled === args.enabled) return;
            changes.push({ field: 'want_enabled', before: r.want_enabled, after: args.enabled, name: r.name, id: r.id });
            r.want_enabled = args.enabled;
          });
          mockPushUpdate(changes);
          return Promise.resolve(args.ids.length);
        }
        case 'set_rules_category': {
          if (categories.indexOf(args.category) < 0) categories.push(args.category);
          const changes = [];
          args.ids.forEach(function (id) {
            const r = find(id);
            if (!r || r.category === args.category) return;
            changes.push({ field: 'category', before: r.category, after: args.category, name: r.name, id: r.id });
            r.category = args.category;
          });
          mockPushUpdate(changes);
          return Promise.resolve(args.ids.length);
        }
        case 'set_rules_hidden':
          args.ids.forEach(function (id) { const r = find(id); if (r) r.hidden = args.hidden; });
          return Promise.resolve(rules.filter(function (r) { return r.hidden; }).length);
        case 'capture_acl': { const r = find(args.id); if (r) r.has_acl = true; return Promise.resolve('D:(A;;FA;;;SY)'); }
        case 'clear_acl': { const r = find(args.id); if (r) r.has_acl = false; return Promise.resolve(null); }
        case 'water_poll':
          return Promise.resolve([
            { root_id: 1, label: 'RAMDisk 4G', path: 'R:', used_pct: 70.0, free: 1288490188, free_text: '1.2 GB', total_text: '4.0 GB', level: 'ok', message: tf('mock.water_msg', { pct: '70.0', free: '1.2 GB' }) },
          ]);
        case 'preset_rules':
          return Promise.resolve([
            { name: 'Chrome 网页缓存', category: 'browser', source_template: '%LOCALAPPDATA%\\Google\\Chrome\\User Data\\Default\\Cache', target_subpath: 'browsers\\chrome-cache', note: null, source: 'C:\\Users\\me\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Cache', exists: true },
            { name: 'Edge 网页缓存', category: 'browser', source_template: '%LOCALAPPDATA%\\Microsoft\\Edge\\User Data\\Default\\Cache', target_subpath: 'browsers\\edge-cache', note: null, source: 'C:\\Users\\me\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\Cache', exists: true },
            { name: 'npm 缓存', category: 'dev', source_template: '%LOCALAPPDATA%\\npm-cache', target_subpath: 'dev\\npm-cache', note: null, source: 'C:\\Users\\me\\AppData\\Local\\npm-cache', exists: true },
            { name: '用户 TEMP', category: 'system', source_template: '%LOCALAPPDATA%\\Temp', target_subpath: 'system\\user-temp', note: '体积最大也最容易被占用：应用时被占的文件会自动跳过', source: 'C:\\Users\\me\\AppData\\Local\\Temp', exists: true, redirect_mode: 'setting', env_vars: ['TEMP', 'TMP'], env_scope: 'user', setting_options: [{ scope: 'user', vars: ['TEMP', 'TMP'] }] },
            { name: '微信网页缓存', category: 'office', source_template: '%APPDATA%\\Tencent\\xwechat\\radium\\web', target_subpath: 'office\\wechat-radium-web', note: '只迁这个子目录；整个 xwechat 里有聊天记录，切勿整体迁移', source: 'C:\\Users\\me\\AppData\\Roaming\\Tencent\\xwechat\\radium\\web', exists: true },
          ]);
        case 'clean_preview': {
          const tgt = args.target || {};
          // 保留类型：mock 里按「配了类型就额外跳过 5 条」演示（真实计数由 core 算）。
          const types = Array.isArray(args.typeExcludes) ? args.typeExcludes.length : 0;
          return Promise.resolve({
            dir: tgt.kind === 'folder' ? tgt.path : 'R:\\dev\\vscode-cache',
            label: '（模拟）VS Code 缓存',
            files: 1280, bytes: 536870912, bytes_text: '512.0 MB', links_skipped: 0,
            excluded_skipped: (tgt.kind === 'rule' ? 12 : 0) + (types ? 5 : 0),
            sample: [
              { path: 'Cache\\f1', bytes: 1048576, mtime: Math.floor(Date.now() / 1000) - 86400 * 3 },
              { path: 'Cache\\f2', bytes: 2097152, mtime: Math.floor(Date.now() / 1000) - 3600 * 5 },
            ],
            summary: '1280 个文件 / 512.0 MB',
          });
        }
        case 'clean_run':
          return Promise.resolve({
            dir: 'R:\\dev\\vscode-cache', label: '（模拟）VS Code 缓存',
            deleted_files: 1280, deleted_bytes: 536870912, deleted_bytes_text: '512.0 MB',
            dirs_pruned: 3, skipped: 2, skip_examples: ['f3', 'f4'], links_skipped: 0,
            excluded_skipped: 12 + ((args.typeExcludes || []).length ? 5 : 0),
            summary: '已删除 1280 个文件 / 512.0 MB，剪除 3 个空目录；2 个文件被占用或无权限已跳过（例如：f3；f4）',
          });
        case 'inspect_source': {
          const tpl = String(args.template || '');
          const expanded = tpl.replace('%LOCALAPPDATA%', 'C:\\Users\\me\\AppData\\Local').replace('%APPDATA%', 'C:\\Users\\me\\AppData\\Roaming');
          const blocked = /^[a-z]:\\windows/i.test(expanded);
          return Promise.resolve({
            expanded: expanded, exists: tpl.indexOf('%NO_SUCH') < 0,
            blocking: blocked ? ['该路径属于系统关键目录，禁止重定向：' + expanded] : [],
            warnings: /program files/i.test(tpl) ? ['该路径位于受保护位置，移动/建链需要管理员权限'] : [],
            requires_elevation: /program files|programdata/i.test(tpl),
            unknown_vars: /%NO_SUCH/.test(tpl) ? ['NO_SUCH'] : [],
          });
        }
        case 'inspect_target': {
          const sub = String(args.subpath || '').replace(/^[\\\/]+/, '');
          const busy = /busy|占用/.test(sub);
          return Promise.resolve({
            target: 'R:\\' + sub, root_ready: true, exists: busy, occupied: busy,
            state: busy ? '未链接（真实目录）' : '源路径不存在', unknown_vars: [],
          });
        }
        case 'apply_rules': {
          const out = [];
          args.ids.forEach(function (id) {
            const r = find(id);
            if (!r) { out.push({ rule_id: id, name: '#' + id, ok: false, action: 'not_found', message: '不存在', elapsed_ms: 0 }); return; }
            if (r.needs_elevation || /ps-media|vscode/i.test(r.target_subpath)) {
              out.push({ rule_id: id, name: r.name, ok: false, action: 'failed', message: '源目录正被占用，已中止（未做任何改动）：3 个文件被占用，占用者：WeChat.exe（PID 8024）', elapsed_ms: 0 });
              return;
            }
            // 「系统设置模式」：改的是环境变量，不建链，文案也不同。
            if (r.redirect_mode === 'setting') {
              r._state = 'setting_ok'; r.badge = '✅'; r.label = stateLabel(r); r.needs_attention = false;
              out.push({
                rule_id: id, name: r.name, ok: true, action: 'setting_applied',
                message: '已把 ' + (r.env_vars || []).join('/') + ' 指向 ' + r.target, elapsed_ms: 42,
              });
              return;
            }
            r._state = 'link_ok'; r.badge = '✅'; r.label = stateLabel(r); r.needs_attention = false;
            out.push({ rule_id: id, name: r.name, ok: true, action: 'moved_and_linked', message: '同卷重命名：120 个文件 / 4.0 MB；已建链', elapsed_ms: 42 });
          });
          if (MOCK) simulateProgress(args.ids.length);
          return Promise.resolve(out);
        }
        case 'restore_rules': {
          const out = [];
          args.ids.forEach(function (id) {
            const r = find(id);
            if (!r) return;
            r._state = 'plain_dir'; r.badge = '❌'; r.label = stateLabel(r); r.needs_attention = true;
            out.push({ rule_id: id, name: r.name, ok: true, action: 'restored', message: '已恢复：120 个文件 / 4.0 MB 已搬回', elapsed_ms: 31 });
          });
          return Promise.resolve(out);
        }
        case 'relink_rule': {
          const r = find(args.id);
          if (r && r.badge !== '✅') { r._state = 'link_ok'; r.badge = '✅'; r.label = stateLabel(r); r.needs_attention = false; }
          return Promise.resolve({ rule_id: args.id, name: r ? r.name : '', ok: true, action: 'relinked', message: '链接已重建', elapsed_ms: 8 });
        }
        case 'check_locks':
          return Promise.resolve({
            locked_files: ['C:\\Users\\me\\AppData\\Roaming\\Tencent\\WeChat\\FileStorage\\msg\\a.bin', 'C:\\Users\\me\\AppData\\Roaming\\Tencent\\WeChat\\FileStorage\\msg\\b.bin'],
            denied_files: [], blockers: [{ app: 'WeChat.exe', pid: 8024, is_service: false }],
            scanned: 1284, truncated: false, elapsed_ms: 63,
          });
        case 'disk_usage': {
          const rows = rules.map(function (r) {
            const bytes = r.last_bytes || 0;
            return {
              id: r.id, name: r.name, dir: r.target,
              files: r.last_files || 0, bytes: bytes, bytes_text: humanBytes(bytes),
              exists: bytes > 0,
            };
          }).sort(function (a, b) { return b.bytes - a.bytes; });
          const totalBytes = rows.reduce(function (s, r) { return s + r.bytes; }, 0);
          return Promise.resolve({
            rules: rows,
            volumes: [
              { path: 'R:\\', total: 4294967296, free: 1288490188, used: 3006477108, total_text: '4.0 GB', free_text: '1.2 GB', used_text: '2.8 GB', used_pct: 70.0, is_ramdisk: true, roles: ['RAM 盘'] },
              { path: 'C:\\', total: 511101108224, free: 128849018880, used: 382252089344, total_text: '476.0 GB', free_text: '120.0 GB', used_text: '356.0 GB', used_pct: 74.8, is_ramdisk: false, roles: ['源盘'] },
            ],
            total_bytes: totalBytes, total_bytes_text: humanBytes(totalBytes),
            rule_count: rules.length, elapsed_ms: 42,
          });
        }
        case 'boot_rebuild':
          return Promise.resolve({
            roots: [{ root_id: 1, label: 'RAMDisk 4G', path: 'R:', ready: true, waited_ms: 120, error: null }],
            rules_total: 8, dirs_created: 8, links_recovered: 6, links_repaired: 0, already_ok: 2,
            warnings: ['Photoshop 暂存盘：源路径已变回真实目录（可能被软件更新覆盖），未自动处理，请在界面上重新应用'],
            failed: [], elapsed_ms: 37,
          });
        case 'autostart_status': {
          const exe = 'C:\\Program Files\\SubLink\\sublink-gui.exe';
          const normalCmd = exe + ' boot-rebuild --skip-elevated';
          const elevCmd = autostart.elevated ? exe + ' boot-rebuild --elevated-only' : '';
          const startupCmd = autostart.startup ? exe + ' boot-rebuild --setting-only' : '';
          return Promise.resolve({
            registered: true,
            task_name: 'SubLink-BootRebuild',
            command: normalCmd,
            message: tf('mock.autostart_registered', { cmd: normalCmd }),
            elevated_registered: autostart.elevated,
            elevated_task_name: 'SubLink-BootRebuild-Elevated',
            elevated_command: elevCmd,
            elevated_message: autostart.elevated
              ? tf('mock.autostart_registered', { cmd: elevCmd })
              : t('auto.elev_hint'),
            startup_registered: autostart.startup,
            startup_task_name: 'SubLink-BootRebuild-Startup',
            startup_command: startupCmd,
            startup_message: autostart.startup
              ? tf('mock.autostart_registered', { cmd: startupCmd })
              : t('auto.startup_hint'),
            needs_update: autostart.needsUpdate,
            elevated: false,
            data_dir: 'C:\\ProgramData\\SubLink',
            extra_args: '',
            recommended_exe: exe,
          });
        }
        case 'autostart_enable': return Promise.resolve('C:\\Program Files\\SubLink\\sublink-gui.exe boot-rebuild --skip-elevated');
        case 'autostart_disable': return Promise.resolve('已取消开机自启');
        case 'autostart_run_now': return Promise.resolve('已触发一次开机重建任务');
        case 'autostart_elevated_enable':
          autostart.elevated = true;
          return Promise.resolve('C:\\Program Files\\SubLink\\sublink-gui.exe boot-rebuild --elevated-only');
        case 'autostart_elevated_disable':
          autostart.elevated = false;
          return Promise.resolve('已取消开机自启');
        case 'autostart_startup_enable':
          autostart.startup = true;
          return Promise.resolve('C:\\Program Files\\SubLink\\sublink-gui.exe boot-rebuild --setting-only');
        case 'autostart_startup_disable':
          autostart.startup = false;
          return Promise.resolve('已取消系统启动时任务');
        case 'run_elevated_job': {
          const out = [];
          (args.ids || []).forEach(function (id) {
            const r = find(id);
            if (!r) return;
            r._state = 'link_ok'; r.badge = '✅'; r.label = stateLabel(r); r.needs_attention = false;
            out.push({ rule_id: id, name: r.name, ok: true, action: 'moved_and_linked', message: '（提权）跨卷复制：120 个文件 / 4.0 MB；已建链', elapsed_ms: 88 });
          });
          return Promise.resolve(out);
        }
        case 'export_config': return Promise.resolve([1, rules.length]);
        case 'import_config': return Promise.resolve({ roots_reused: 1, roots_created: 0, rules_created: 5, rules_skipped: 2, rules_hidden: 2, errors: ['第 3 条规则「Photoshop 暂存盘」未导入：已存在指向同一路径的规则'], dry_run: false });
        case 'pick_folder': return Promise.resolve(null);
        case 'pick_save_file': return Promise.resolve(null);
        case 'pick_open_file': return Promise.resolve(null);
        case 'reveal_in_explorer': return Promise.resolve(null);
        case 'show_main_window': return Promise.resolve(null);
        case 'quit_app': return Promise.resolve(null);
        case 'ui_settings': return Promise.resolve({ close_to_tray: mockCloseToTray, allow_merge: mockAllowMerge });
        case 'set_close_to_tray':
          mockCloseToTray = !!args.enabled;
          return Promise.resolve(null);
        case 'set_allow_merge':
          mockAllowMerge = !!args.enabled;
          return Promise.resolve(null);
        case 'scheduled_clean_settings':
          return Promise.resolve({ enabled: mockSched.enabled, time: mockSched.time });
        case 'set_scheduled_clean': {
          const tm = (args.time || '').trim();
          if (!/^\d{2}:\d{2}$/.test(tm)) return Promise.reject(new Error(t('toast.bad_time_format')));
          mockSched = { enabled: !!args.enabled, time: tm };
          return Promise.resolve(null);
        }
        case 'scheduled_clean_poll':
          // mock 永远"未到点"（避免预览界面真的删东西的错觉）
          return Promise.resolve({ ran: false, done: 0, failed: 0, skipped: 0, deleted_files: 0, deleted_bytes: 0, deleted_text: '0 B' });
        case 'set_locale':
          try { localStorage.setItem(LOCALE_KEY, args.locale); } catch (e) { /* 忽略 */ }
          // 与真机一致：语言切换后状态文案按新语言重算（随后 sl-lang-change 会触发重渲染）
          relabelRules();
          return Promise.resolve(null);
        case 'get_locale':
          return Promise.resolve(getLang());
        default: return Promise.reject(new Error('mock 未实现命令：' + name));
      }
    },
    listen: function () { return Promise.resolve(function () {}); },
  };
}

function simulateProgress(total) {
  let i = 0;
  showProgress(tf('mock.simulate_running', { n: total }));
  const timer = setInterval(function () {
    i += 1;
    setProgress((i / total) * 100, tf('mock.simulate_detail', { i: i, n: total, files: i * 4200, mb: i * 137 }));
    if (i >= total) clearInterval(timer);
  }, 260);
}

// ---------------------------------------------------------------------------
// 语言切换
// ---------------------------------------------------------------------------

// 语言切换分段按钮（中文 | EN）：初值取自 i18n（localStorage，无记录默认英文）；
// 切换时通知后端并整体刷新。
//
// 为什么要整体 refresh 而不是只 render()：规则状态标签 / 横幅 / 水位消息这些文案是
// 后端按当前语言生成后随快照下发的，只重渲染 DOM 会用旧语言的缓存数据 ——
// 必须重新拉一次快照，切换后的列表才整片换语言（mock 后端在 set_locale 里原地重算，
// 走的是同一条契约）。
(function initLang() {
  const wrap = $('lang-switch');
  if (wrap) {
    const paint = function () {
      wrap.querySelectorAll('.lang-seg').forEach(function (b) {
        const on = b.dataset.lang === getLang();
        b.classList.toggle('active', on);
        b.setAttribute('aria-pressed', on ? 'true' : 'false');
      });
    };
    wrap.querySelectorAll('.lang-seg').forEach(function (b) {
      b.addEventListener('click', function () { setLang(b.dataset.lang); });
    });
    paint();
    window.addEventListener('sl-lang-change', function (ev) {
      const lang = ev.detail.lang;
      invoke('set_locale', { locale: lang })
        .catch(function () { /* mock 或旧后端可忽略 */ })
        .then(function () { return refresh(true); })
        .catch(function () { if (state.snap) render(); });
      paint();
    });
  }
})();

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

(async function boot() {
  // 启动即把界面语言同步给后端：状态标签 / 横幅 / 水位消息等文案由后端按当前语言
  // 生成，不推送的话后端停留在自己的默认语言（中文），界面是英文、数据是中文。
  try { await invoke('set_locale', { locale: getLang() }); } catch (e) { /* mock 或旧后端 */ }
  await refresh(false);
  if (PARAMS.has('progress')) {
    showProgress(t('mock.progress_title'));
    setProgress(46, t('mock.progress_detail'));
  }
  if (PARAMS.has('modal') || PARAMS.has('add')) {
    startDraftRule();
  }
  if (PARAMS.has('settings')) {
    await openSettings();
  }
  if (PARAMS.has('ops')) {
    await openOps();
  }
  if (PARAMS.has('disk')) {
    await openDiskStats();
  }
  if (PARAMS.has('clean')) {
    await openBatchCleanDialog();
  }

  // RAM 盘水位巡检（60s）：级别变化才落事件（后端去抖）；
  // 发现 warn/critical 顺带刷新快照，让概览横幅及时出现。
  // 定时（低峰期）清理与水位同节拍巡检：到点且当天没跑过才执行（后端判定），
  // 真的删了文件时由 Rust 侧发系统通知，这里只需在执行后刷新快照。
  setInterval(async function () {
    try {
      const levels = await invoke('water_poll', {});
      if (levels.some(function (w) { return w.level === 'warn' || w.level === 'critical'; })) {
        await refresh(true);
      }
      const sc = await invoke('scheduled_clean_poll', {});
      if (sc.ran) await refresh(true);
    } catch (e) { /* 巡检失败静默，下轮再试 */ }
  }, 60000);
})();
