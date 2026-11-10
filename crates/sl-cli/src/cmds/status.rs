//! 体检与观测：doctor / status / locks / volume / events / ops。

use std::collections::HashMap;
use std::path::PathBuf;

use sl_core::boot::{probe_volume, wait_for_volume};
use sl_core::engine;
use sl_core::fsops::occupancy::{scan_locks, LockScanOptions};
use sl_core::i18n::{t, tf};
use sl_core::link::detect::LinkState;
use sl_core::paths;
use sl_core::store::Store;
use sl_core::{SlError, SlResult};

use crate::output::print_status_line;
use crate::parse::Parsed;
use crate::timefmt::{local_time_of_day_secs, now_utc_secs};

pub(crate) fn cmd_version() -> SlResult<()> {
    println!("SubLink {}", env!("CARGO_PKG_VERSION"));
    println!("{}", t("cli.version.data_root_hint"));
    Ok(())
}

pub(crate) fn cmd_doctor(store: &Store) -> SlResult<()> {
    println!("{}", t("cli.doctor.env_title"));
    let data_dir = paths::data_dir().display().to_string();
    println!("{}", tf("cli.doctor.data_dir", &[("path", data_dir.as_str())]));
    let db_path = paths::db_path().display().to_string();
    println!("{}", tf("cli.doctor.db_path", &[("path", db_path.as_str())]));
    let uv = store.user_version()?.to_string();
    println!("{}", tf("cli.doctor.schema", &[("v", uv.as_str())]));
    println!("{}", tf("cli.doctor.platform", &[("os", std::env::consts::OS)]));
    let who = if sl_core::elevate::is_elevated() {
        t("cli.doctor.admin")
    } else {
        t("cli.doctor.normal_user")
    };
    println!("{}", tf("cli.doctor.proc_identity", &[("who", who)]));
    println!();
    println!("{}", t("cli.doctor.link_selftest_title"));
    match junction_selftest() {
        Ok(note) => println!("✅ {note}"),
        Err(e) => println!("❌ {e}"),
    }
    println!();

    println!("{}", t("cli.doctor.roots_title"));
    let roots = store.list_roots()?;
    if roots.is_empty() {
        println!("{}", t("cli.doctor.roots_empty"));
    }
    for r in &roots {
        let (p, unknown) = r.resolved_path();
        let (state, msg) = probe_volume(&p);
        let icon = match state {
            "ready" => "✅",
            "missing" => "❌",
            _ => "⚠️",
        };
        let unknown_part = if unknown.is_empty() {
            String::new()
        } else {
            tf(
                "cli.doctor.unresolved_vars",
                &[("vars", &unknown.join(t("cli.doctor.var_sep")))],
            )
        };
        println!(
            "{}",
            tf(
                "cli.doctor.root_row",
                &[
                    ("icon", icon),
                    ("id", &r.id.to_string()),
                    ("label", r.label.as_str()),
                    ("path", r.path.as_str()),
                    ("state", state),
                    ("msg", msg.as_str()),
                    ("unknown", unknown_part.as_str()),
                ],
            ),
        );
    }
    println!();

    println!("{}", t("cli.doctor.water_title"));
    let levels = sl_core::usecase::water_levels(store)?;
    if levels.is_empty() {
        println!("{}", t("cli.doctor.water_empty"));
    }
    for w in &levels {
        let icon = match w.level.as_str() {
            "ok" => "✅",
            "warn" => "⚠️",
            "critical" => "❌",
            _ => "❓",
        };
        println!(
            "{}",
            tf(
                "cli.doctor.water_row",
                &[
                    ("icon", icon),
                    ("id", &w.root_id.to_string()),
                    ("label", w.label.as_str()),
                    ("path", w.path.as_str()),
                    ("msg", w.message.as_str()),
                ],
            ),
        );
    }
    println!();

    println!("{}", t("cli.doctor.rules_title"));
    let rules = store.list_rules()?;
    println!(
        "{}",
        tf("cli.doctor.rules_count", &[("n", &rules.len().to_string())])
    );
    let mut by_state: HashMap<String, usize> = HashMap::new();
    let mut needs = 0usize;
    for rule in &rules {
        let st = engine::rule_status(store, rule)?;
        *by_state
            .entry(engine::state_key(&st.state).to_string())
            .or_default() += 1;
        if st.needs_attention {
            needs += 1;
        }
    }
    for (k, v) in by_state.iter() {
        println!("  {:<20} {v}", k);
    }
    println!(
        "{}",
        tf("cli.doctor.needs_attention", &[("n", &needs.to_string())])
    );
    println!();

    print_sysenv_section(store);

    let open = store.list_open_ops()?;
    println!("{}", t("cli.doctor.open_title"));
    if open.is_empty() {
        println!("{}", t("cli.doctor.open_none"));
    } else {
        for op in open {
            let rule = format!("{:?}", op.rule_id);
            let err = op.error.unwrap_or_else(|| t("cli.doctor.err_none").to_string());
            println!(
                "{}",
                tf(
                    "cli.doctor.open_row",
                    &[
                        ("id", &op.id.to_string()),
                        ("rule", rule.as_str()),
                        ("kind", op.kind.as_str()),
                        ("phase", op.phase.as_str()),
                        ("err", err.as_str()),
                    ],
                ),
            );
        }
    }
    Ok(())
}

/// 「系统设置模式」相关变量的自检段。
///
/// 为什么单独一段：这个模式改的是**系统环境变量**，出了问题的表现和链接模式
/// 完全不同 —— 链接坏了界面会显示 ❌，而变量被改歪了要对着注册表才看得出来。
/// 用户排查时最需要知道的是三件事：
/// 1. 每个白名单变量**当前指向哪儿**（注册表原值，不是进程里的快照）；
/// 2. 它的**注册表类型**是什么（`REG_EXPAND_SZ` 被降级成 `REG_SZ` 是真问题 ——
///    展开的 `%VAR%` 会失效）；
/// 3. 规则**期望**它指向哪儿（不一致就是"有人手工改过"）。
fn print_sysenv_section(store: &Store) {
    println!("{}", t("cli.doctor.sysenv_title"));

    let rules = match store.list_rules() {
        Ok(r) => r,
        Err(e) => {
            println!("  ❌ {e}");
            return;
        }
    };
    // 规则登记的「变量 → 期望值」：键是 "作用域|变量名"（作用域用字符串，
    // 免得给 EnvScope 加 Hash 派生），值是 (期望路径, 规则名)。
    let mut expected: HashMap<String, (String, String)> = HashMap::new();
    for rule in &rules {
        if !sl_core::engine::is_setting_mode(rule) {
            continue;
        }
        let Some(scope) = sl_core::sysenv::EnvScope::parse(&rule.env_scope) else {
            continue;
        };
        let Ok(vars) = sl_core::engine::rule_vars(rule) else {
            continue;
        };
        let Ok(resolved) = sl_core::engine::resolve(store, rule) else {
            continue;
        };
        let want = resolved.target.display().to_string();
        for v in vars {
            expected.insert(format!("{}|{}", scope.as_str(), v), (want.clone(), rule.name.clone()));
        }
    }

    for scope in [sl_core::sysenv::EnvScope::User, sl_core::sysenv::EnvScope::Machine] {
        println!(
            "{}",
            tf("cli.doctor.sysenv_scope", &[("scope", scope.label())])
        );
        // 只列**白名单内**的变量 —— 别的变量本工具不管，列出来只会误导。
        for name in ["TEMP", "TMP"] {
            let raw = sl_core::sysenv::read_env(scope, name);
            let (icon, value_part) = match &raw {
                Ok(Some(v)) => (
                    "✅",
                    tf(
                        "cli.doctor.sysenv_var",
                        &[("value", v.raw.as_str()), ("type", v.type_name())],
                    ),
                ),
                Ok(None) => (
                    "⚠️",
                    t("cli.doctor.sysenv_var_absent").to_string(),
                ),
                Err(e) => ("❌", e.to_string()),
            };

            // 与规则期望对一下：不一致说明有人手工改过变量（或规则登记错了）。
            let expect_part = match expected.get(&format!("{}|{}", scope.as_str(), name)) {
                Some((want, rule_name)) => {
                    // 判定走 sysenv::env_points_to —— 与 engine::setting_state 同一个入口，
                    // 两处的路径比较规则不会漂移。
                    let hit = sl_core::sysenv::env_points_to(
                        scope,
                        name,
                        &std::path::PathBuf::from(want),
                    );
                    if hit {
                        tf(
                            "cli.doctor.sysenv_expect_ok",
                            &[("rule", rule_name.as_str())],
                        )
                    } else {
                        tf(
                            "cli.doctor.sysenv_expect_mismatch",
                            &[("rule", rule_name.as_str()), ("want", want.as_str())],
                        )
                    }
                }
                None => String::new(),
            };

            // 变量名左对齐补宽（i18n 的 tf 只做 {name} 替换，不支持宽度格式）。
            let padded = format!("{:<14}", name);
            println!(
                "{}",
                tf(
                    "cli.doctor.sysenv_row",
                    &[
                        ("icon", icon),
                        ("name", padded.as_str()),
                        ("value", value_part.as_str()),
                        ("expect", expect_part.as_str()),
                    ],
                ),
            );
        }
    }

    if expected.is_empty() {
        println!("{}", t("cli.doctor.sysenv_no_rules"));
    }
    // 说明为什么只有这两个变量 —— 用户最容易问的就是"为什么不能改 LOCALAPPDATA"。
    println!("{}", t("cli.doctor.sysenv_whitelist_note"));
}

/// 用临时目录做一次真实的 Junction 建/删，确认这台机器确实支持。
fn junction_selftest() -> SlResult<String> {
    let base = std::env::temp_dir().join(format!("td_selftest_{}", std::process::id()));
    let target = base.join("target");
    let link = base.join("link");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&target)?;

    let result = (|| -> SlResult<String> {
        sl_core::link::create_dir_link(&link, &target, sl_core::link::LinkType::Junction)?;
        let state = sl_core::link::detect_link(&link, Some(&target));
        let note = match &state {
            LinkState::LinkOk { .. } => t("cli.doctor.junction_ok").to_string(),
            other => {
                let label = other.label();
                tf("cli.doctor.junction_abnormal", &[("label", label.as_str())])
            }
        };
        sl_core::link::remove_dir_link(&link)?;
        Ok(note)
    })();

    let _ = std::fs::remove_dir_all(&base);
    result
}

pub(crate) fn cmd_status(store: &Store, p: &Parsed) -> SlResult<()> {
    let category = p.value("category");
    let search = p.value("search");
    let enabled_only = p.has("enabled") && !p.has("all");
    let persist = !p.has("no-persist");

    let list = engine::status_all(store, category, enabled_only, search, persist)?;
    if p.json() {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("{}", t("cli.status.no_match"));
        return Ok(());
    }

    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for st in &list {
        *counts.entry(st.badge).or_default() += 1;
    }
    let ok = counts.get("✅").copied().unwrap_or(0);
    let warn = counts.get("⚠️").copied().unwrap_or(0);
    let crit = counts.get("❌").copied().unwrap_or(0);
    let other = list.len() - ok - warn - crit;
    println!(
        "{}",
        tf(
            "cli.status.summary",
            &[
                ("total", &list.len().to_string()),
                ("ok", &ok.to_string()),
                ("warn", &warn.to_string()),
                ("crit", &crit.to_string()),
                ("other", &other.to_string()),
            ],
        ),
    );
    println!();
    for (i, st) in list.iter().enumerate() {
        print_status_line(i, st);
    }
    Ok(())
}

pub(crate) fn cmd_locks(store: &Store, p: &Parsed) -> SlResult<()> {
    let target = p.required(0, "<规则id | 目录路径>")?;
    let dir: PathBuf = match target.parse::<i64>() {
        Ok(id) => {
            let rule = store.get_rule(id)?;
            let r = engine::resolve(store, &rule)?;
            r.source
        }
        Err(_) => PathBuf::from(&target),
    };
    if !dir.is_dir() {
        return Err(SlError::InvalidPath(format!(
            "不是目录：{}",
            dir.display()
        )));
    }
    let dir_s = dir.display().to_string();
    println!("{}", tf("cli.locks.scanning", &[("dir", dir_s.as_str())]));
    let report = scan_locks(&dir, &LockScanOptions::default())?;
    if p.json() {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!(
        "{}",
        tf(
            "cli.locks.scanned",
            &[
                ("n", &report.scanned.to_string()),
                ("ms", &report.elapsed_ms.to_string()),
            ],
        ),
    );
    if report.is_clear() {
        println!("{}", t("cli.locks.clear"));
        return Ok(());
    }
    println!("⚠️ {}", report.summary());
    for b in &report.blockers {
        let who = b.describe();
        println!("{}", tf("cli.locks.blocker", &[("who", who.as_str())]));
    }
    if !report.locked_files.is_empty() {
        println!("{}", t("cli.locks.locked_files"));
        for f in report.locked_files.iter().take(5) {
            println!("       {}", f.display());
        }
    }
    if !report.denied_files.is_empty() {
        println!("{}", t("cli.locks.denied_files"));
        for f in report.denied_files.iter().take(5) {
            println!("       {}", f.display());
        }
    }
    Ok(())
}

pub(crate) fn cmd_volume(p: &Parsed) -> SlResult<()> {
    let target = p.required(0, "<盘符或目录，如 R: 或 R:\\Redir>")?;
    let path = PathBuf::from(&target);
    let timeout = p
        .value("timeout")
        .map(|v| v.parse::<u64>())
        .transpose()
        .map_err(|_| SlError::Other("--timeout 必须是整数毫秒".into()))?
        .unwrap_or(0);
    if timeout == 0 {
        let (state, msg) = probe_volume(&path);
        println!("{state}: {msg}");
        return Ok(());
    }
    println!(
        "{}",
        tf(
            "cli.volume.waiting",
            &[("target", target.as_str()), ("timeout", &timeout.to_string())],
        ),
    );
    let waited = wait_for_volume(
        &path,
        std::time::Duration::from_millis(timeout),
        std::time::Duration::from_millis(200),
    )?;
    let ms = waited.as_millis().to_string();
    println!("{}", tf("cli.volume.ready", &[("ms", ms.as_str())]));
    Ok(())
}

/// `water`：一次水位巡检（与 GUI 定时器同源）。**会落事件**：级别变化才落，
/// 平稳期重复执行零副作用 —— 适合放计划任务低频跑。
/// `water sweep <策略> [--yes]`：水位一键清理 —— 对全部规则按统一策略
/// 预演/批量执行（等价 GUI 横幅上的「一键清理」按钮，破坏性操作同样两步走）。
pub(crate) fn cmd_water(store: &Store, p: &Parsed) -> SlResult<()> {
    if p.pos(0) == Some("sweep") {
        return water_sweep(store, p);
    }
    let levels = sl_core::usecase::water_poll(store)?;
    if p.json() {
        println!("{}", serde_json::to_string_pretty(&levels)?);
        return Ok(());
    }
    if levels.is_empty() {
        println!("{}", t("cli.water.empty"));
        return Ok(());
    }
    for w in &levels {
        let icon = match w.level.as_str() {
            "ok" => "✅",
            "warn" => "⚠️",
            "critical" => "❌",
            _ => "❓",
        };
        println!("{icon} #{} {}  {}  {}", w.root_id, w.label, w.path, w.message);
    }
    Ok(())
}

fn water_sweep(store: &Store, p: &Parsed) -> SlResult<()> {
    use sl_core::usecase::{clean_preview, clean_run, CleanTarget};

    let now_utc = now_utc_secs();
    let tod = local_time_of_day_secs() as i64;
    let policy = crate::cmds::clean::build_clean_policy(p, now_utc, tod)?;
    let rules = store.list_rules()?;
    if rules.is_empty() {
        println!("{}", t("cli.water.sweep_no_rules"));
        return Ok(());
    }

    if !p.has("yes") {
        let mut files = 0u64;
        let mut bytes = 0u64;
        for r in &rules {
            let pv = clean_preview(store, &CleanTarget::Rule { id: r.id }, &policy, &[])?;
            files += pv.files;
            bytes += pv.bytes;
            if pv.files > 0 {
                let id_col = format!("{:<3}", r.id);
                println!(
                    "{}",
                    tf(
                        "cli.water.sweep_row",
                        &[
                            ("name", r.name.as_str()),
                            ("id", id_col.as_str()),
                            ("summary", pv.summary.as_str()),
                            ("dir", pv.dir.as_str()),
                        ],
                    ),
                );
            }
        }
        println!();
        let policy_desc = crate::cmds::clean::describe_policy(&policy, now_utc);
        println!(
            "{}",
            tf("cli.water.sweep_policy", &[("policy", policy_desc.as_str())])
        );
        if files == 0 {
            println!("{}", t("cli.clean.no_files"));
        } else {
            let size = sl_core::util::human_bytes(bytes);
            println!(
                "{}",
                tf(
                    "cli.water.sweep_total",
                    &[
                        ("files", &files.to_string()),
                        ("size", size.as_str()),
                        ("rules", &rules.len().to_string()),
                    ],
                ),
            );
            println!();
            println!("{}", t("cli.clean.dry_run_hint"));
        }
        return Ok(());
    }

    let quiet = p.json();
    let mut deleted_files = 0u64;
    let mut deleted_bytes = 0u64;
    let mut failed = 0u32;
    let total = rules.len();
    for (i, r) in rules.iter().enumerate() {
        if !quiet {
            print!(
                "{}",
                tf(
                    "cli.water.sweep_progress",
                    &[
                        ("i", &(i + 1).to_string()),
                        ("total", &total.to_string()),
                        ("name", r.name.as_str()),
                    ],
                ),
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        match clean_run(
            store,
            &CleanTarget::Rule { id: r.id },
            &policy,
            &[],
            false,
            &mut |_| {},
        ) {
            Ok(rep) => {
                deleted_files += rep.deleted_files;
                deleted_bytes += rep.deleted_bytes;
            }
            Err(_) => failed += 1,
        }
    }
    if !quiet {
        println!();
        let size = sl_core::util::human_bytes(deleted_bytes);
        let extra = if failed > 0 {
            tf("cli.water.sweep_failed_extra", &[("n", &failed.to_string())])
        } else {
            String::new()
        };
        println!(
            "{}",
            tf(
                "cli.water.sweep_done",
                &[
                    ("files", &deleted_files.to_string()),
                    ("size", size.as_str()),
                    ("extra", extra.as_str()),
                ],
            ),
        );
    }
    Ok(())
}

/// `stats`：效益统计 —— 三个可解释的数字（当前重定向 / 累计搬运 / 累计清理），
/// 数据来自 operations 表结构化列，口径与 GUI 运维弹窗一致（usecase/stats.rs）。
pub(crate) fn cmd_stats(store: &Store, p: &Parsed) -> SlResult<()> {
    let v = sl_core::usecase::savings(store)?;
    if p.json() {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("{}", t("cli.stats.title"));
    println!(
        "{}",
        tf(
            "cli.stats.redirect",
            &[
                ("rules", &v.redirected_rules.to_string()),
                ("files", &v.redirected_files.to_string()),
                ("size", v.redirected_text.as_str()),
            ],
        ),
    );
    println!(
        "{}",
        tf(
            "cli.stats.moved",
            &[("files", &v.moved_files.to_string()), ("size", v.moved_text.as_str())],
        ),
    );
    println!(
        "{}",
        tf(
            "cli.stats.cleaned",
            &[
                ("files", &v.cleaned_files.to_string()),
                ("size", v.cleaned_text.as_str()),
            ],
        ),
    );
    Ok(())
}

pub(crate) fn cmd_events(store: &Store, p: &Parsed) -> SlResult<()> {
    let limit = p
        .value("limit")
        .map(|v| v.parse::<i64>())
        .transpose()
        .map_err(|_| SlError::Other("--limit 必须是整数".into()))?
        .unwrap_or(30);
    for e in store.recent_events(limit)? {
        println!(
            "{} [{}] {:<20} {}",
            e.at,
            e.level,
            e.kind,
            e.message
        );
    }
    Ok(())
}

pub(crate) fn cmd_ops(store: &Store, p: &Parsed) -> SlResult<()> {
    let limit = p
        .value("limit")
        .map(|v| v.parse::<i64>())
        .transpose()
        .map_err(|_| SlError::Other("--limit 必须是整数".into()))?
        .unwrap_or(20);
    let open = store.list_open_ops()?;
    if !open.is_empty() {
        println!(
            "{}",
            tf("cli.ops.open_header", &[("n", &open.len().to_string())])
        );
        for op in &open {
            let rule = format!("{:?}", op.rule_id);
            let err = op
                .error
                .clone()
                .unwrap_or_else(|| t("cli.doctor.err_none").to_string());
            println!(
                "{}",
                tf(
                    "cli.ops.open_row",
                    &[
                        ("id", &op.id.to_string()),
                        ("rule", rule.as_str()),
                        ("kind", op.kind.as_str()),
                        ("phase", op.phase.as_str()),
                        ("err", err.as_str()),
                    ],
                ),
            );
        }
        println!();
    }
    for op in store.recent_ops(limit)? {
        println!(
            "{} #{:<4} {:<10} {:<12} files={} bytes={} {}{}",
            op.started_at,
            op.id,
            op.kind,
            op.phase,
            op.file_count.unwrap_or(0),
            op.bytes.unwrap_or(0),
            op.source_path.clone().unwrap_or_default(),
            op.error
                .as_ref()
                .map(|e| format!("  ERR: {e}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}