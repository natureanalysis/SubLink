//! 端到端验证：外置 `presets.json` 覆盖内置预设。
//!
//! 单元测试只分别测了合并逻辑（`merge_presets`）和文件读取（`load_user_presets`），
//! 这条走**真实链路**：写一个外置 presets.json → 用 `SUBLINK_DATA_DIR` 把数据目录
//! 指过去 → 调 `preset_rules()`，确认内置「用户 TEMP」被外置条目覆盖，
//! 且不会残留同名内置项（覆盖而非追加）。
//!
//! ⚠️ 集成测试是独立进程，`set_var` 只影响本进程，不会污染 lib 里那条
//! 依赖真实数据目录的 `preset_rules_filters_by_existence`。
//!
//! 但**同一二进制内的多个 `#[test]` 默认并行跑**，两条都要改同一个环境变量，
//! 所以用 `ENV_LOCK` 把「设 env → 调 preset_rules → 清 env」这段临界区串行化，
//! 否则会出现一个测试刚设好目录、另一个又覆盖、结果读到对方文件的竞态。

/// 串行化对进程级环境变量 `SUBLINK_DATA_DIR` 的读改写。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// 每次调用发一个唯一序号，保证两个测试各用各的临时目录（清理不会互删）。
static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 在临时数据目录里放好 presets.json，跑一次 `preset_rules()`，再清理。
/// 全程持锁 + 唯一目录：既避免并行的两个测试互相踩环境变量，也避免共享目录被
/// 对方的清理误删（曾经踩过：目录同名 + 清理在锁外 → 读到内置回落值）。
fn preset_rules_with_external(file_content: &[u8]) -> Vec<sl_gui::api::PresetView> {
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sl_gui_preset_{}_{}", std::process::id(), n));

    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("presets.json"), file_content).unwrap();
    // data_dir() 命中环境变量时**直接返回其值**（不追加 SubLink），所以文件放根下。
    std::env::set_var(sl_core::paths::DATA_DIR_ENV, &dir);
    let out = sl_gui::api::preset_rules();
    std::env::remove_var(sl_core::paths::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// 覆盖内置「用户 TEMP」：source_template 必须逐字一致才会命中覆盖。
#[test]
fn external_presets_json_overrides_builtin() {
    // 用 json! 构造，转义交给 serde，避免手写反斜杠踩坑。
    let user = serde_json::json!([
        {
            "name": "用户 TEMP（已被外置覆盖）",
            "category": "system",
            "source_template": "%LOCALAPPDATA%\\Temp",
            "target_subpath": "system\\temp-external",
            "note": "外置覆盖验证"
        }
    ]);
    let out = preset_rules_with_external(&serde_json::to_vec_pretty(&user).unwrap());

    // %LOCALAPPDATA%\Temp 在任意 Windows 上都存在，覆盖条目必然出现在过滤结果里。
    let temp_entries: Vec<_> = out
        .iter()
        .filter(|v| v.source_template == r"%LOCALAPPDATA%\Temp")
        .collect();
    assert_eq!(
        temp_entries.len(),
        1,
        "覆盖后 %LOCALAPPDATA%\\Temp 只应有一条（内置被替换、非追加）：{temp_entries:?}"
    );
    assert_eq!(temp_entries[0].name, "用户 TEMP（已被外置覆盖）");
    assert_eq!(temp_entries[0].target_subpath, r"system\temp-external");
    assert_eq!(temp_entries[0].note.as_deref(), Some("外置覆盖验证"));
}

/// 外置文件语法坏时，`preset_rules()` 必须回落到内置、绝不整体失效。
#[test]
fn broken_external_presets_falls_back_to_builtin() {
    let out = preset_rules_with_external(b"{ not valid json");
    assert!(
        out.iter().any(|v| v.source_template == r"%LOCALAPPDATA%\Temp"),
        "外置文件坏了也应保留内置预设，实际命中 {} 条",
        out.len()
    );
}
