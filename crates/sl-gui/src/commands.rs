//! `#[tauri::command]` 薄包装层。
//!
//! 这一层只做三件事：取状态锁、调用 [`crate::api`]、把错误转成字符串。
//! **不放任何业务逻辑** —— 业务逻辑全在 `api.rs`，那里可以脱离窗口做单元测试。

use std::collections::BTreeMap;

use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, State};

use sl_core::store::Store;

use crate::api;
use crate::notify;

/// settings 键：关闭窗口时是否驻留托盘（`"0"` = 直接退出；缺省/其它 = 驻留）。
pub const KEY_CLOSE_TO_TRAY: &str = "close_to_tray";

/// settings 键：应用时是否允许把源目录**合并进已有内容的目标目录**
/// （`"1"` = 允许；缺省 / 其它 = 不允许）。
pub const KEY_ALLOW_MERGE: &str = "apply_allow_merge";

/// 全局状态。`Store` 不是 `Sync`，用 `Mutex` 包一层；
/// 命令是同步的（跑在 Tauri 的工作线程上），因此不会卡住界面线程，
/// 但同一时刻只有一个写操作在跑 —— 这正是我们想要的。
///
/// `init_error` 记录"打开真实数据库失败、已降级到内存库"的原因，
/// 由 `ui_snapshot` 作为错误横幅显示出来（不能静默降级，用户会以为一切正常）。
pub struct AppState {
    store: Mutex<Store>,
    init_error: Option<String>,
    /// 水位告警去重：`root_id` → 上次已通知的级别。
    /// 只在"升级到 critical"时通知一次；降级后清掉记录，下次越线可再通知。
    water_notified: Mutex<BTreeMap<i64, String>>,
}

impl AppState {
    pub fn new(store: Store, init_error: Option<String>) -> Self {
        Self {
            store: Mutex::new(store),
            init_error,
            water_notified: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn init_error(&self) -> Option<&str> {
        self.init_error.as_deref()
    }
}

/// 读"关闭窗口时驻留托盘"偏好；缺省（未设置）= 驻留。
pub fn close_to_tray_setting(store: &Store) -> bool {
    store
        .get_setting(KEY_CLOSE_TO_TRAY)
        .ok()
        .flatten()
        .map(|v| v.trim() != "0")
        .unwrap_or(true)
}

/// 读"应用时允许合并到已有目标目录"偏好；缺省（未设置）= **不允许**。
///
/// 默认关：合并会把源目录并进**已经有内容**的目标目录，同名文件取修改时间
/// 较新者 —— 也就是目标里的旧文件可能被源的新版本覆盖。这是用户该主动
/// 打开的开关，不是默认行为。
pub fn allow_merge_setting(store: &Store) -> bool {
    store
        .get_setting(KEY_ALLOW_MERGE)
        .ok()
        .flatten()
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// 关闭窗口时是否驻留托盘。
///
/// 拿不到锁时按**安全默认（驻留）**：宁可让用户多点一次托盘退出，
/// 也不要因为一次瞬时锁竞争就把后台巡检 / 预授权自动清理一起关掉。
pub fn close_to_tray(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    // 先把结果取出来再返回：`try_lock` 的临时 guard 会在 match 表达式末尾析构，
    // 若直接作为块尾表达式返回，会和局部 `state` 的析构顺序冲突（E0597）。
    let stay = match state.store.try_lock() {
        Ok(store) => close_to_tray_setting(&store),
        Err(_) => true,
    };
    stay
}


/// 进度事件名（Tauri 只允许字母数字与 `-/:_`，所以用连字符）。
pub const EV_PROGRESS: &str = "td-progress";
pub const EV_DONE: &str = "td-done";
/// 「磁盘空间占用统计」扫描进度（逐条规则回吐）。
pub const EV_DISK_PROGRESS: &str = "td-disk-progress";

/// 取状态锁。公开是因为托盘线程也要用同一把锁。
pub fn lock_store<'a>(
    state: &'a State<'_, AppState>,
) -> Result<std::sync::MutexGuard<'a, Store>, String> {
    state
        .store
        .lock()
        .map_err(|_| "内部状态锁已损坏（前一次操作 panic 了），请重启程序".to_string())
}

type R<T> = Result<T, String>;

fn map_err(e: sl_core::SlError) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// 读
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn ui_snapshot(state: State<'_, AppState>) -> R<api::UiSnapshot> {
    let mut snap = api::snapshot(&*lock_store(&state)?).map_err(map_err)?;
    if let Some(err) = state.init_error() {
        snap.banners.insert(
            0,
            api::Banner {
                level: "error",
                text: format!(
                    "数据目录初始化失败，当前运行在临时内存库（本次改动不会保存）：{err}"
                ),
                action: None,
            },
        );
    }
    Ok(snap)
}

#[tauri::command]
pub fn recent_events(state: State<'_, AppState>, limit: i64) -> R<Vec<sl_core::store::EventRow>> {
    api::recent_events(&*lock_store(&state)?, limit).map_err(map_err)
}

#[tauri::command]
pub fn recent_ops(state: State<'_, AppState>, limit: i64) -> R<Vec<sl_core::store::Operation>> {
    api::recent_ops(&*lock_store(&state)?, limit).map_err(map_err)
}

#[tauri::command]
pub fn savings(state: State<'_, AppState>) -> R<api::SavingsView> {
    api::savings(&*lock_store(&state)?).map_err(map_err)
}

// ---------------------------------------------------------------------------
// 目标根
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn add_root(
    state: State<'_, AppState>,
    label: String,
    path: String,
    is_ramdisk: bool,
    wait_timeout_ms: i64,
) -> R<i64> {
    api::add_root(
        &*lock_store(&state)?,
        &label,
        &path,
        is_ramdisk,
        wait_timeout_ms,
    )
    .map_err(map_err)
}

#[tauri::command]
pub fn delete_root(state: State<'_, AppState>, id: i64) -> R<()> {
    api::delete_root(&*lock_store(&state)?, id).map_err(map_err)
}

// ---------------------------------------------------------------------------
// 规则
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn add_rule(state: State<'_, AppState>, form: api::RuleForm) -> R<i64> {
    api::add_rule(&*lock_store(&state)?, &form).map_err(map_err)
}

#[tauri::command]
pub fn update_rule(state: State<'_, AppState>, id: i64, form: api::RuleForm) -> R<()> {
    api::update_rule(&*lock_store(&state)?, id, &form).map_err(map_err)
}

#[tauri::command]
pub fn delete_rules(state: State<'_, AppState>, ids: Vec<i64>) -> R<usize> {
    api::delete_rules(&*lock_store(&state)?, &ids).map_err(map_err)
}

/// 撤销最近一次操作：删除（把被删掉的规则记录搬回规则库）或修改（把字段改回去）。
///
/// 只搬记录 —— 删除本来就没碰磁盘上的目录与链接，所以撤销不需要提权、不搬数据。
/// 唯一的例外是「目标」被改回去了：这时要把托管链接指回旧目标（后端自动做，
/// 失败也只是提示，用户可以在界面上「重建链接」补上）。
#[tauri::command]
pub fn undo_last(state: State<'_, AppState>) -> R<api::UndoResult> {
    let out = api::undo_last(&*lock_store(&state)?).map_err(map_err)?;
    Ok(api::UndoResult {
        restored: out.restored,
        skipped: out.skipped,
        kind: out.op.unwrap_or_default(),
    })
}

#[tauri::command]
pub fn set_rules_enabled(state: State<'_, AppState>, ids: Vec<i64>, enabled: bool) -> R<usize> {
    api::set_enabled(&*lock_store(&state)?, &ids, enabled).map_err(map_err)
}

/// 批量归类：把勾选的规则统一移到指定分类（分类可以是内置的，也可以是新建的）。
#[tauri::command]
pub fn set_rules_category(state: State<'_, AppState>, ids: Vec<i64>, category: String) -> R<usize> {
    api::set_category(&*lock_store(&state)?, &ids, &category).map_err(map_err)
}

/// 隐藏 / 取消隐藏列表条目（纯展示偏好，存在 settings，不写 rules 表）。
#[tauri::command]
pub fn set_rules_hidden(state: State<'_, AppState>, ids: Vec<i64>, hidden: bool) -> R<usize> {
    api::set_hidden(&*lock_store(&state)?, &ids, hidden).map_err(map_err)
}

#[tauri::command]
pub fn reorder_rules(state: State<'_, AppState>, ordered_ids: Vec<i64>) -> R<usize> {
    api::reorder_rules(&*lock_store(&state)?, &ordered_ids).map_err(map_err)
}

#[tauri::command]
pub fn capture_acl(state: State<'_, AppState>, id: i64) -> R<String> {
    api::capture_acl(&*lock_store(&state)?, id).map_err(map_err)
}

#[tauri::command]
pub fn clear_acl(state: State<'_, AppState>, id: i64) -> R<()> {
    api::clear_acl(&*lock_store(&state)?, id).map_err(map_err)
}

#[tauri::command]
pub fn inspect_source(template: String) -> R<api::SourceInspection> {
    api::inspect_source(&template).map_err(map_err)
}

/// 内置预设（常用软件缓存目录），只返回本机实际存在的条目。
#[tauri::command]
pub fn preset_rules() -> Vec<api::PresetView> {
    api::preset_rules()
}

#[tauri::command]
pub fn inspect_target(
    state: State<'_, AppState>,
    root_id: i64,
    subpath: String,
) -> R<api::TargetInspection> {
    api::inspect_target(&*lock_store(&state)?, root_id, &subpath).map_err(map_err)
}

// ---------------------------------------------------------------------------
// 应用 / 恢复（带进度事件）
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn apply_rules(
    app: AppHandle,
    state: State<'_, AppState>,
    ids: Vec<i64>,
    opts: api::ApplyOpts,
) -> R<Vec<api::ItemOutcome>> {
    let guard = lock_store(&state)?;
    let app2 = app.clone();
    let out = api::apply_rules(&guard, &ids, &opts, &mut move |i, total, name, p| {
        let _ = app2.emit(
            EV_PROGRESS,
            serde_json::json!({
                "index": i, "total": total, "name": name,
                "phase": progress_phase(p),
                "detail": progress_detail(p),
            }),
        );
    });
    let ok = out.iter().filter(|o| o.ok).count();
    let failed = out.len() - ok;
    let _ = app.emit(
        EV_DONE,
        serde_json::json!({ "kind": "apply", "ok": ok, "failed": failed }),
    );
    Ok(out)
}

#[tauri::command]
pub fn restore_rules(
    app: AppHandle,
    state: State<'_, AppState>,
    ids: Vec<i64>,
    opts: api::ApplyOpts,
) -> R<Vec<api::ItemOutcome>> {
    let guard = lock_store(&state)?;
    let app2 = app.clone();
    let out = api::restore_rules(&guard, &ids, &opts, &mut move |i, total, name, p| {
        let _ = app2.emit(
            EV_PROGRESS,
            serde_json::json!({
                "index": i, "total": total, "name": name,
                "phase": progress_phase(p),
                "detail": progress_detail(p),
            }),
        );
    });
    let ok = out.iter().filter(|o| o.ok).count();
    let failed = out.len() - ok;
    let _ = app.emit(
        EV_DONE,
        serde_json::json!({ "kind": "restore", "ok": ok, "failed": failed }),
    );
    Ok(out)
}

#[tauri::command]
pub fn relink_rule(state: State<'_, AppState>, id: i64) -> R<api::ItemOutcome> {
    api::relink(&*lock_store(&state)?, id).map_err(map_err)
}

#[tauri::command]
pub fn check_locks(
    state: State<'_, AppState>,
    id: i64,
) -> R<sl_core::fsops::occupancy::LockReport> {
    api::check_locks(&*lock_store(&state)?, id).map_err(map_err)
}

// ---------------------------------------------------------------------------
// 文件夹清理（预演 / 执行）
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn clean_preview(
    state: State<'_, AppState>,
    target: api::CleanTarget,
    policy: sl_core::fsops::clean::CleanPolicy,
    type_excludes: Option<Vec<String>>,
) -> R<api::CleanPreviewView> {
    let type_excludes = type_excludes.unwrap_or_default();
    api::clean_preview(&*lock_store(&state)?, &target, &policy, &type_excludes).map_err(map_err)
}

#[tauri::command]
pub fn clean_run(
    app: AppHandle,
    state: State<'_, AppState>,
    target: api::CleanTarget,
    policy: sl_core::fsops::clean::CleanPolicy,
    dry_run: bool,
    type_excludes: Option<Vec<String>>,
) -> R<api::CleanResultView> {
    let type_excludes = type_excludes.unwrap_or_default();
    let guard = lock_store(&state)?;
    let app2 = app.clone();
    let out = api::clean_run(&guard, &target, &policy, &type_excludes, dry_run, &mut move |p| {
        let _ = app2.emit(
            EV_PROGRESS,
            serde_json::json!({
                "index": 0, "total": 1, "name": "清理文件夹",
                "phase": progress_phase(p), "detail": progress_detail(p),
            }),
        );
    })
    .map_err(map_err)?;
    let _ = app.emit(
        EV_DONE,
        serde_json::json!({ "kind": "clean", "ok": out.deleted_files, "failed": out.skipped }),
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// 磁盘空间占用统计
// ---------------------------------------------------------------------------

/// 实时扫描各规则缓存目录的占用 + 相关卷容量。会逐条发射扫描进度。
#[tauri::command]
pub fn disk_usage(app: AppHandle, state: State<'_, AppState>) -> R<api::DiskUsageView> {
    let guard = lock_store(&state)?;
    let app2 = app.clone();
    api::disk_usage(&guard, &mut move |i, total, name| {
        let _ = app2.emit(
            EV_DISK_PROGRESS,
            serde_json::json!({ "index": i, "total": total, "name": name }),
        );
    })
    .map_err(map_err)
}

// ---------------------------------------------------------------------------
// RAM 盘水位监控
// ---------------------------------------------------------------------------

/// 定时巡检水位：级别变化才落事件（前端定时器调用，见 app.js 启动段）。
///
/// 顺带把两类"人不在界面前也要知道"的事推送成系统通知：
/// 水位**升级到 critical**、以及**预授权自动清理真的删了文件**。
#[tauri::command]
pub fn water_poll(app: AppHandle, state: State<'_, AppState>) -> R<Vec<api::WaterLevel>> {
    let levels = {
        let store = lock_store(&state)?;
        // 记录调用前的最新事件 id，用来识别"本次巡检新产生的"事件。
        let before = store
            .recent_events(1)
            .ok()
            .and_then(|v| v.first().map(|e| e.id))
            .unwrap_or(0);
        let levels = api::water_poll(&store).map_err(map_err)?;
        // 自动清理是破坏性动作，不能悄悄发生（事件由 run_auto_clean 落）。
        if let Ok(evs) = store.recent_events(10) {
            if let Some(e) = evs.iter().find(|e| e.id > before && e.kind == "auto_clean") {
                notify::notify(
                    &app,
                    if e.level == "warn" {
                        "水位自动清理有失败"
                    } else {
                        "水位自动清理完成"
                    },
                    &e.message,
                );
            }
        }
        levels
    };
    notify_water_critical(&app, &state, &levels);
    Ok(levels)
}

/// 水位升级到 critical 时发一次系统通知（同根同级别不重复）。
///
/// 只在 critical 通知：warn 已在界面上有横幅，也推通知会迅速让人麻木 ——
/// 通知是给"窗口关着"的场景用的，必须稀缺才有效。
fn notify_water_critical(app: &AppHandle, state: &State<'_, AppState>, levels: &[api::WaterLevel]) {
    let mut notified = match state.water_notified.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    for lv in pick_new_critical(&mut notified, levels) {
        notify::notify(
            app,
            "RAM 盘即将写满",
            &format!(
                "{}（{}）已用 {:.1}%，仅剩 {}。软件写缓存可能开始失败，建议清理。",
                lv.label, lv.path, lv.used_pct, lv.free_text
            ),
        );
    }
}

/// 挑出"本次应当通知"的水位项，并就地更新去重表。
///
/// 规则：某根**新升级到 critical** 才返回（同级别不重复）；非 critical 的根
/// 清掉记录，这样降级恢复后再越线仍能通知。抽成纯函数是为了能脱离 Tauri
/// 运行时测这条"通知疲劳"边界。
fn pick_new_critical<'a>(
    notified: &mut BTreeMap<i64, String>,
    levels: &'a [api::WaterLevel],
) -> Vec<&'a api::WaterLevel> {
    let mut out = Vec::new();
    for lv in levels {
        if lv.level == "critical" {
            if notified.get(&lv.root_id).map(String::as_str) != Some("critical") {
                notified.insert(lv.root_id, lv.level.clone());
                out.push(lv);
            }
        } else {
            notified.remove(&lv.root_id);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 开机重建 / 迁移 / 其它
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn boot_rebuild(app: AppHandle, state: State<'_, AppState>) -> R<sl_core::boot::BootReport> {
    // 锁只在重建期间持有，通知放到锁外发（否则一次系统调用会挡住其它命令）。
    let report = {
        let guard = lock_store(&state)?;
        api::boot_rebuild(&guard).map_err(map_err)?
    };
    let _ = app.emit(
        EV_DONE,
        serde_json::json!({ "kind": "boot", "ok": report.ok_count(), "failed": report.failed.len() }),
    );
    // 重建是"软件启动前"的关键一步，出问题要主动叫醒用户，不能只留在界面里。
    if report.has_problem() {
        notify::notify(&app, "开机重建未完全成功", &report.one_line());
    }
    Ok(report)
}

#[tauri::command]
pub fn export_config(state: State<'_, AppState>, path: String) -> R<(usize, usize)> {
    api::export_config(&*lock_store(&state)?, std::path::Path::new(&path)).map_err(map_err)
}

#[tauri::command]
pub fn import_config(
    state: State<'_, AppState>,
    path: String,
    ram_root: Option<String>,
    dry_run: bool,
) -> R<sl_core::portable::ImportReport> {
    api::import_config(
        &*lock_store(&state)?,
        std::path::Path::new(&path),
        ram_root.as_deref(),
        dry_run,
    )
    .map_err(map_err)
}

/// 系统目录选择框（走 Tauri dialog 插件的 Rust API，前端不依赖 npm 包）。
#[tauri::command]
pub fn pick_folder(
    app: AppHandle,
    title: String,
    default_path: Option<String>,
) -> R<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    let mut fb = app.dialog().file().set_title(&title);
    if let Some(p) = default_path.filter(|p| !p.trim().is_empty()) {
        let path = std::path::PathBuf::from(&p);
        if path.is_dir() {
            fb = fb.set_directory(path);
        }
    }
    Ok(fb
        .blocking_pick_folder()
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.display().to_string()))
}

#[tauri::command]
pub fn pick_save_file(app: AppHandle, title: String, default_name: String) -> R<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    Ok(app
        .dialog()
        .file()
        .set_title(&title)
        .set_file_name(&default_name)
        .add_filter("JSON 配置", &["json"])
        .blocking_save_file()
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.display().to_string()))
}

#[tauri::command]
pub fn pick_open_file(app: AppHandle, title: String) -> R<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    Ok(app
        .dialog()
        .file()
        .set_title(&title)
        .add_filter("JSON 配置", &["json"])
        .blocking_pick_file()
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.display().to_string()))
}

/// 在资源管理器里定位（用系统自带 explorer，不引额外依赖）。
#[tauri::command]
pub fn reveal_in_explorer(path: String) -> R<()> {
    let p = std::path::Path::new(&path);
    let target = if p.is_dir() {
        p.to_path_buf()
    } else {
        match p.parent() {
            Some(parent) if parent.is_dir() => parent.to_path_buf(),
            _ => return Err(format!("路径不存在，无法打开：{path}")),
        }
    };
    std::process::Command::new("explorer")
        .arg(target.as_os_str())
        .spawn()
        .map_err(|e| format!("调起资源管理器失败：{e}"))?;
    Ok(())
}

#[tauri::command]
pub fn show_main_window(app: AppHandle) -> R<()> {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    Ok(())
}

/// 收起主窗口（关闭 → 驻留托盘时由前端在提示露出之后调用）。
#[tauri::command]
pub fn hide_main_window(app: AppHandle) -> R<()> {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    Ok(())
}

#[tauri::command]
pub fn quit_app(app: AppHandle) -> R<()> {
    app.exit(0);
    Ok(())
}

// ---------------------------------------------------------------------------
// 界面偏好
// ---------------------------------------------------------------------------

/// 界面偏好（关闭行为 + 应用时的合并策略；后续偏好项往这里加）。
#[derive(serde::Serialize)]
pub struct UiSettings {
    /// 关闭窗口时隐藏到托盘而不是退出（默认 true）。
    pub close_to_tray: bool,
    /// 应用时允许把源目录合并进**已有内容**的目标目录（默认 false）。
    pub allow_merge: bool,
}

#[tauri::command]
pub fn ui_settings(state: State<'_, AppState>) -> R<UiSettings> {
    let store = lock_store(&state)?;
    Ok(UiSettings {
        close_to_tray: close_to_tray_setting(&store),
        allow_merge: allow_merge_setting(&store),
    })
}

#[tauri::command]
pub fn set_close_to_tray(state: State<'_, AppState>, enabled: bool) -> R<()> {
    lock_store(&state)?
        .set_setting(KEY_CLOSE_TO_TRAY, if enabled { "1" } else { "0" })
        .map_err(map_err)
}

/// 打开 / 关闭「应用时合并到已有目标目录」。
///
/// 只有 `"1"` 算开启（严格匹配）：宁可解析得保守，也不能因为脏数据
/// 让一次合并在用户不知情时发生。
#[tauri::command]
pub fn set_allow_merge(state: State<'_, AppState>, enabled: bool) -> R<()> {
    lock_store(&state)?
        .set_setting(KEY_ALLOW_MERGE, if enabled { "1" } else { "0" })
        .map_err(map_err)
}

/// 前端切换语言后写回内核全局语言状态（错误 / 提示文案随之换语言）。
/// 无法识别的标签静默忽略，保留当前语言。
#[tauri::command]
pub fn set_locale(locale: String) {
    if let Some(l) = sl_core::i18n::Locale::parse(&locale) {
        sl_core::i18n::set_locale(l);
    }
}

/// 读取内核当前语言标签（前端首屏对齐用）。
#[tauri::command]
pub fn get_locale() -> String {
    sl_core::i18n::locale().tag().to_string()
}

// ---------------------------------------------------------------------------
// 定时（低峰期）自动清理
// ---------------------------------------------------------------------------

/// 定时清理巡检（前端 60s 定时器与水位巡检同节拍调用）。
/// 真的执行了才通知：删了文件说"完成"，有失败说"有失败" ——
/// 破坏性动作不能悄悄发生（与水位自动清理的通知原则一致）。
#[tauri::command]
pub fn scheduled_clean_poll(
    app: AppHandle,
    state: State<'_, AppState>,
) -> R<api::ScheduledCleanView> {
    let v = {
        let store = lock_store(&state)?;
        api::scheduled_clean_poll(&store).map_err(map_err)?
    };
    if v.ran && (v.deleted_files > 0 || v.failed > 0) {
        let (title, msg) = if v.failed > 0 {
            (
                "定时自动清理有失败",
                format!(
                    "{} 条预授权规则执行失败（详见运维日志），本次共删除 {} 个文件 / {}",
                    v.failed, v.deleted_files, v.deleted_text
                ),
            )
        } else {
            (
                "定时自动清理完成",
                format!(
                    "低峰期例行清理 {} 条预授权规则：删除 {} 个文件 / {}",
                    v.done, v.deleted_files, v.deleted_text
                ),
            )
        };
        notify::notify(&app, title, &msg);
    }
    Ok(v)
}

/// 定时清理设置（设置弹窗读）。
#[derive(serde::Serialize)]
pub struct ScheduledCleanSettings {
    pub enabled: bool,
    /// "HH:MM"（缺省 03:00）。
    pub time: String,
}

#[tauri::command]
pub fn scheduled_clean_settings(state: State<'_, AppState>) -> R<ScheduledCleanSettings> {
    let store = lock_store(&state)?;
    let enabled = store
        .get_setting(api::schedule::KEY_ENABLED)
        .ok()
        .flatten()
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let time = store
        .get_setting(api::schedule::KEY_TIME)
        .ok()
        .flatten()
        .unwrap_or_else(|| api::schedule::DEFAULT_TIME.to_string());
    Ok(ScheduledCleanSettings { enabled, time })
}

#[tauri::command]
pub fn set_scheduled_clean(state: State<'_, AppState>, enabled: bool, time: String) -> R<()> {
    let t = time.trim();
    if api::schedule::parse_hhmm(t).is_none() {
        return Err(format!("时刻格式应为 HH:MM（如 03:00），收到：{t:?}"));
    }
    let store = lock_store(&state)?;
    store
        .set_setting(api::schedule::KEY_ENABLED, if enabled { "1" } else { "0" })
        .map_err(map_err)?;
    store
        .set_setting(api::schedule::KEY_TIME, t)
        .map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 开机自启 / 提权（M3）
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn autostart_status(state: State<'_, AppState>) -> R<api::AutostartView> {
    Ok(api::autostart_status(&*lock_store(&state)?))
}

#[tauri::command]
pub fn autostart_enable() -> R<String> {
    api::autostart_enable().map_err(map_err)
}

#[tauri::command]
pub fn autostart_disable() -> R<String> {
    api::autostart_disable().map_err(map_err)
}

#[tauri::command]
pub fn autostart_run_now() -> R<String> {
    api::autostart_run_now().map_err(map_err)
}

/// 注册"最高权限登录任务"（会弹一次 UAC）。
#[tauri::command]
pub fn autostart_elevated_enable() -> R<String> {
    api::autostart_elevated_enable().map_err(map_err)
}

/// 取消"最高权限登录任务"（会弹一次 UAC）。
#[tauri::command]
pub fn autostart_elevated_disable() -> R<String> {
    api::autostart_elevated_disable().map_err(map_err)
}

/// 注册"系统启动时任务"（SYSTEM 身份，登录前重建系统设置模式规则的目录；会弹一次 UAC）。
#[tauri::command]
pub fn autostart_startup_enable() -> R<String> {
    api::autostart_startup_enable().map_err(map_err)
}

/// 取消"系统启动时任务"（会弹一次 UAC）。
#[tauri::command]
pub fn autostart_startup_disable() -> R<String> {
    api::autostart_startup_disable().map_err(map_err)
}

/// 走提权 worker 执行 apply / restore（会弹一次 UAC）。
#[tauri::command]
pub fn run_elevated_job(
    app: AppHandle,
    state: State<'_, AppState>,
    kind: String,
    ids: Vec<i64>,
    opts: api::ApplyOpts,
) -> R<Vec<api::ItemOutcome>> {
    let guard = lock_store(&state)?;
    let out = api::run_elevated_job(&guard, &kind, &ids, &opts).map_err(map_err)?;
    let ok = out.iter().filter(|o| o.ok).count();
    let failed = out.len() - ok;
    let _ = app.emit(
        EV_DONE,
        serde_json::json!({ "kind": kind, "ok": ok, "failed": failed, "elevated": true }),
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// 进度事件的小工具
// ---------------------------------------------------------------------------

use sl_core::fsops::mover::Progress;

fn progress_phase(p: &Progress) -> &'static str {
    match p {
        Progress::Enumerated { .. } => "enumerated",
        Progress::Renaming { .. } => "renaming",
        Progress::Copying { .. } => "copying",
        Progress::Verifying => "verifying",
        Progress::Deleting { .. } => "deleting",
        Progress::Linking { .. } => "linking",
        Progress::Done { .. } => "done",
    }
}

fn progress_detail(p: &Progress) -> String {
    match p {
        Progress::Enumerated { files, bytes } => format!(
            "待处理 {} 个文件 / {}",
            files,
            sl_core::util::human_bytes(*bytes)
        ),
        Progress::Renaming { .. } => "同卷重命名（瞬时）".into(),
        Progress::Copying { files, bytes, .. } => format!(
            "已复制 {} 个文件 / {}",
            files,
            sl_core::util::human_bytes(*bytes)
        ),
        Progress::Verifying => "正在核对数据".into(),
        Progress::Deleting { files } => format!("已删除 {files} 个文件"),
        Progress::Linking { .. } => "正在建立链接".into(),
        Progress::Done { files, bytes } => format!(
            "完成：{} 个文件 / {}",
            files,
            sl_core::util::human_bytes(*bytes)
        ),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use sl_core::usecase::WaterLevel;

    fn level(root_id: i64, lv: &str) -> WaterLevel {
        WaterLevel {
            root_id,
            label: format!("根{root_id}"),
            path: format!("R{root_id}:"),
            used_pct: 96.0,
            free: 1024,
            free_text: "1 KB".into(),
            total_text: "1 GB".into(),
            level: lv.into(),
            message: String::new(),
        }
    }

    #[test]
    fn water_critical_notifies_only_on_upgrade() {
        let mut notified = BTreeMap::new();

        // 首次 critical → 通知一次
        let first = [level(1, "critical")];
        let hit = pick_new_critical(&mut notified, &first);
        assert_eq!(hit.len(), 1, "首次越线要通知");
        assert_eq!(hit[0].root_id, 1);

        // 仍 critical → 不重复（防通知疲劳）
        assert!(pick_new_critical(&mut notified, &first).is_empty());

        // warn / ok 不通知，并清掉记录
        let w = [level(1, "warn")];
        assert!(pick_new_critical(&mut notified, &w).is_empty());
        let o = [level(1, "ok")];
        assert!(pick_new_critical(&mut notified, &o).is_empty());

        // 降级后再越线 → 重新通知
        assert_eq!(pick_new_critical(&mut notified, &first).len(), 1);

        // 多根互不影响：根1 已在"已通知"状态、根2 首次越线，只通知根2
        let two = [level(1, "critical"), level(2, "critical")];
        let hit = pick_new_critical(&mut notified, &two);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].root_id, 2, "只通知新越线的那个根");
    }

    #[test]
    fn close_to_tray_defaults_true_and_persists() {
        let s = Store::open_in_memory().unwrap();
        assert!(close_to_tray_setting(&s), "缺省 = 驻留托盘");

        s.set_setting(KEY_CLOSE_TO_TRAY, "0").unwrap();
        assert!(!close_to_tray_setting(&s), "设 0 = 直接退出");

        s.set_setting(KEY_CLOSE_TO_TRAY, "1").unwrap();
        assert!(close_to_tray_setting(&s));

        // 非 "0" 的值一律视为开启（宽容解析，避免脏数据把后台能力关掉）
        s.set_setting(KEY_CLOSE_TO_TRAY, "yes").unwrap();
        assert!(close_to_tray_setting(&s));
    }

    /// 合并开关**默认关**，且只认严格的 `"1"`。
    ///
    /// 合并会覆盖目标目录里的同名旧文件 —— 解析放宽（像 close_to_tray 那样
    /// "非 0 即真"）会让一次覆盖在用户不知情时发生。
    #[test]
    fn allow_merge_defaults_off_and_requires_explicit_one() {
        let s = Store::open_in_memory().unwrap();
        assert!(!allow_merge_setting(&s), "缺省 = 不合并");

        s.set_setting(KEY_ALLOW_MERGE, "1").unwrap();
        assert!(allow_merge_setting(&s), "显式 \"1\" = 合并");

        s.set_setting(KEY_ALLOW_MERGE, "0").unwrap();
        assert!(!allow_merge_setting(&s), "关 = 不合并");

        // 脏数据（"yes" / "true" / "on"）一律按关闭处理 —— 不覆盖用户的文件。
        for junk in ["yes", "true", "on", "enabled", ""] {
            s.set_setting(KEY_ALLOW_MERGE, junk).unwrap();
            assert!(
                !allow_merge_setting(&s),
                "{junk:?} 不应被当成开启，实际为开启"
            );
        }

        // 首尾空白照常解析（这个值是人手工写进 settings 表的，带空格是常事）。
        s.set_setting(KEY_ALLOW_MERGE, " 1 ").unwrap();
        assert!(allow_merge_setting(&s), "带空白的 \"1\" 应视为开启");
    }
}
