//! 开机批量重建：让所有被重定向的缓存目录在软件启动之前就"存在且可写"。
//!
//! ## 时序
//!
//! 必须在目标软件的启动项之前跑完。软件是 `HKCU\Run` 拉起的，它的顺序不可控，
//! 所以正道是**后台服务监听登录事件**（`WTS_SESSION_LOGON`，比 Run 键早），
//! 用该用户的令牌执行本函数 —— 这样 `%APPDATA%` 展开正确、目录 ACL 也对。
//!
//! ## 三种重建动作
//!
//! | 情况 | 动作 |
//! |---|---|
//! | 链接有效，只是目标目录没了 | 建目录 → 链接自动恢复（**这是最常见的情况**） |
//! | 源完全不存在 | 父目录在的话直接把链接建出来，软件装好就能用 |
//! | 源变回了真实目录（软件更新覆盖） | **不自动动它**，报警告交给人处理 |
//!
//! 第三条是刻意的：真实目录里可能有新写入的缓存，自动转成链接意味着要先搬运，
//! 这属于"会动数据"的操作，不应该在开机静默路径里做。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::acl;
use crate::engine::{self, rule_link_type};
use crate::error::SlResult;
use crate::fsops::mover::Progress;
use crate::i18n::{t, tf};
use crate::link::create::{create_dir_link, ensure_dir};
use crate::link::detect::{detect_link, LinkState};
use crate::store::{Root, Rule, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootOutcome {
    pub root_id: i64,
    pub label: String,
    pub path: String,
    pub ready: bool,
    pub waited_ms: u128,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootReport {
    pub roots: Vec<RootOutcome>,
    pub rules_total: usize,
    /// 新建出来的目标目录数（开机守护的核心产出）。
    pub dirs_created: usize,
    /// 因为目录被重建而自动恢复可用的链接数。
    pub links_recovered: usize,
    /// 链接本身丢了、由我们主动重建的链接数。
    pub links_repaired: usize,
    /// 本来就没问题、不需要任何动作的链接数。
    pub already_ok: usize,
    pub warnings: Vec<String>,
    pub failed: Vec<(i64, String)>,
    pub elapsed_ms: u128,
}

impl BootReport {
    pub fn ok_count(&self) -> usize {
        self.links_recovered + self.links_repaired + self.already_ok
    }

    pub fn has_problem(&self) -> bool {
        !self.warnings.is_empty() || !self.failed.is_empty() || self.roots.iter().any(|r| !r.ready)
    }

    pub fn one_line(&self) -> String {
        crate::i18n::tf("boot.one_line", &[
            ("ready", &self.roots.iter().filter(|r| r.ready).count().to_string()),
            ("total", &self.roots.len().to_string()),
            ("rules", &self.rules_total.to_string()),
            ("dirs", &self.dirs_created.to_string()),
            ("recovered", &self.links_recovered.to_string()),
            ("repaired", &self.links_repaired.to_string()),
            ("ok", &self.already_ok.to_string()),
            ("warn", &self.warnings.len().to_string()),
            ("failed", &self.failed.len().to_string()),
            ("ms", &crate::util::human_dur_ms(self.elapsed_ms)),
        ])
    }
}

enum RebuildAction {
    AlreadyPresent,
    Relinked,
    NeedsAttention(String),
}

/// 开机重建的分工范围。
///
/// 登录时注册了两个任务：普通权限任务处理普通源，最高权限任务只处理
/// `C:\Windows` / `Program Files` 这类受保护源。两条任务各写各的报告，
/// 界面把两份都读出来展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootScope {
    /// 全部规则（手动执行 / CLI 默认）。
    All,
    /// 只处理需要管理员权限的源（高权限登录任务）。
    ElevatedOnly,
    /// 只处理普通源（普通登录任务）。
    NonElevatedOnly,
    /// 只处理「系统设置模式」规则 —— **系统启动时**（用户登录前）那一趟。
    ///
    /// 为什么要有这一档：机器级环境变量（`TEMP`/`TMP`）是注册表里的持久值，
    /// 重启后不会丢，但它指向的目录在 RAM 盘上 —— RAM 盘重启后是空的，
    /// 目录要等有人重建才存在。而登录任务发生在**用户登录之后**，
    /// 在那之前（开机自启服务、安装器、更新程序）已经有进程在用 `%TEMP%` 了，
    /// 变量指向一个不存在的目录，写临时文件直接失败。
    ///
    /// 所以这一档只做一件事：把系统设置模式规则的**目标目录**建出来。
    /// 它以 SYSTEM 身份在系统启动时运行，因此**只能碰系统设置模式规则** ——
    /// 链接模式规则的源路径含 `%LOCALAPPDATA%` 之类的变量，
    /// 在 SYSTEM 上下文里会展开成 systemprofile，路径全错。
    SettingOnly,
}

impl BootScope {
    pub fn parse(flag: &str) -> Option<Self> {
        match flag {
            "" | "all" => Some(Self::All),
            "elevated_only" | "elevated-only" => Some(Self::ElevatedOnly),
            "non_elevated_only" | "skip-elevated" => Some(Self::NonElevatedOnly),
            "setting_only" | "setting-only" => Some(Self::SettingOnly),
            _ => None,
        }
    }

    fn accepts(self, elevated: bool) -> bool {
        match self {
            Self::All => true,
            Self::ElevatedOnly => elevated,
            Self::NonElevatedOnly => !elevated,
            // 这一档不走"按权限分工"的逻辑，单独在收集阶段过滤。
            Self::SettingOnly => false,
        }
    }

    fn report_keys(self) -> (&'static str, &'static str) {
        match self {
            Self::ElevatedOnly => ("last_boot_report_elevated", "last_boot_at_elevated"),
            Self::SettingOnly => ("last_boot_report_startup", "last_boot_at_startup"),
            _ => ("last_boot_report", "last_boot_at"),
        }
    }
}

/// 执行一次开机重建。
pub fn run_boot_rebuild(store: &Store) -> SlResult<BootReport> {
    run_boot_rebuild_scoped(store, BootScope::All)
}

/// 按权限分工执行开机重建。
pub fn run_boot_rebuild_scoped(store: &Store, scope: BootScope) -> SlResult<BootReport> {
    let started = Instant::now();
    let mut report = BootReport::default();

    // 高权限任务在非管理员上下文里跑（用户不在 Administrators 组）时，
    // 受保护目录的链接一个也建不出来 —— 提前说清楚，别让用户去猜。
    let elevated = crate::elevate::is_elevated();
    if scope == BootScope::ElevatedOnly && !elevated {
        report.warnings.push(t("boot.not_elevated").into());
    }

    // ---- 1. 等各目标卷就绪 ----
    let roots = store.list_roots()?;
    let mut ready_ids: Vec<i64> = Vec::new();

    for root in &roots {
        let (path, unknown) = root.resolved_path();
        let mut outcome = RootOutcome {
            root_id: root.id,
            label: root.label.clone(),
            path: path.display().to_string(),
            ready: false,
            waited_ms: 0,
            error: None,
        };

        if !unknown.is_empty() {
            let msg = tf("boot.bad_path", &[("vars", &unknown.join("、"))]);
            outcome.error = Some(msg.clone());
            let _ = store.log_event(None, "error", "root_bad_path", &msg);
            report.roots.push(outcome);
            continue;
        }

        let timeout = Duration::from_millis(root.wait_timeout_ms.max(1000) as u64);
        // wait_for_root（而不是 wait_for_volume）：目标根可能是 RAM 盘下的子目录，
        // RAM 盘重启后它自己也消失了 —— 必须"等卷 + 建目录"，不能等目录自己出现。
        match crate::boot::volume::wait_for_root(&path, timeout, Duration::from_millis(500)) {
            Ok(waited) => {
                outcome.ready = true;
                outcome.waited_ms = waited.as_millis();
                ready_ids.push(root.id);
                // 目标根可能是提权身份建出来的 → 确保对应使用者能写进去。
                // 卷根（`R:\`）跳过：那是整块盘的安全策略，改它要么没权限、要么改大了。
                if !acl::is_volume_root(&path) {
                    if let Some(msg) = crate::engine::ensure_dir_access(store, None, &path) {
                        report.warnings.push(msg);
                    }
                }
                let _ = store.log_event(
                    None,
                    "info",
                    "root_ready",
                    &tf("boot.root_ready", &[
                        ("path", &path.display().to_string()),
                        ("secs", &format!("{:.1}", waited.as_secs_f64())),
                    ]),
                );
            }
            Err(e) => {
                outcome.error = Some(e.to_string());
                let _ = store.log_event(None, "error", "root_timeout", &e.to_string());
            }
        }
        report.roots.push(outcome);
    }

    // ---- 2. 收集待处理规则，按目标层级从浅到深排序 ----
    let mut pending: Vec<(Rule, Root)> = Vec::new();
    for rule in store.rules_for_boot()? {
        // 先按权限分工过滤：不属于本任务的规则，**连告警都不该由本任务发** ——
        // 否则两条登录任务对同一条规则各报一遍，界面上就成了"重复计数"。
        // 判定用 `boot_requires_elevation`：它只问"开机重建这一步要不要提权"。
        // 「系统设置模式」在这里一律算**不需要提权** —— 开机重建只建目标目录、
        // 不碰注册表（环境变量是持久值，重启不丢），写 HKLM 的提权需求只发生在
        // apply / restore。若按"整体执行要不要提权"来分，机器级 setting 规则
        // 会被普通任务跳过、而高权限登录任务默认没注册 → 开机后目标目录不再重建，
        // 环境变量指向不存在的目录，新进程写临时文件直接失败。
        if scope == BootScope::SettingOnly {
            // 「系统启动时」那一趟：以 SYSTEM 身份跑，只认系统设置模式规则。
            // 别的规则一律不碰 —— 它们的源路径靠 `%LOCALAPPDATA%` 这类变量展开，
            // 在 SYSTEM 上下文里会展开成 systemprofile，路径是错的。
            if !engine::is_setting_mode(&rule) {
                continue;
            }
        } else if scope != BootScope::All {
            let boot_elevated = engine::boot_requires_elevation(store, &rule);
            if !scope.accepts(boot_elevated) {
                continue;
            }
        }
        if !ready_ids.contains(&rule.target_root_id) {
            // 目标根没就绪 → 这条规则本轮被跳过。**必须显式说出来**：
            // 开机重建是静默跑的，用户看到的只是"目标卷 0/1 就绪"这种卷级信息，
            // 接不上"到底哪条规则没处理"。
            // 「系统设置模式」更严重：环境变量是持久值，变量仍指着这个不存在的目录，
            // 新进程写临时文件会直接失败 —— 链接模式只是链接没恢复，性质完全不同。
            warn_root_not_ready(store, &rule, &mut report.warnings);
            continue;
        }
        let root = store.get_root(rule.target_root_id)?;
        pending.push((rule, root));
    }
    pending.sort_by_key(|(r, _)| r.target_subpath.matches(['\\', '/']).count());
    report.rules_total = pending.len();

    // ---- 3. 逐条重建 ----
    let mut noop = |_: Progress| {};
    let _ = &mut noop;

    for (rule, root) in &pending {
        match rebuild_one(store, rule, root, &mut report.warnings) {
            Ok((created, RebuildAction::AlreadyPresent)) => {
                if created {
                    report.dirs_created += 1;
                    report.links_recovered += 1;
                } else {
                    report.already_ok += 1;
                }
            }
            Ok((created, RebuildAction::Relinked)) => {
                if created {
                    report.dirs_created += 1;
                }
                report.links_repaired += 1;
            }
            Ok((created, RebuildAction::NeedsAttention(w))) => {
                if created {
                    report.dirs_created += 1;
                }
                report.warnings.push(tf("boot.warning", &[
                    ("name", &rule.name),
                    ("msg", &w),
                ]));
            }
            Err(e) => report.failed.push((rule.id, e.to_string())),
        }
    }

    report.elapsed_ms = started.elapsed().as_millis();
    store.log_event(
        None,
        if report.has_problem() { "warn" } else { "info" },
        "boot_rebuild",
        &report.one_line(),
    )?;

    // 落盘一份结果：登录任务是无界面跑的，失败必须能在下次打开界面时看到，
    // 而不是只躺在事件日志里等用户去翻。
    if let Ok(json) = serde_json::to_string(&report) {
        let (report_key, at_key) = scope.report_keys();
        let _ = store.set_setting(report_key, &json);
        let _ = store.set_setting(at_key, &store.now().unwrap_or_default());
    }
    Ok(report)
}

/// 目标根没就绪时，为这条规则补一条点名告警。
///
/// 为什么要单独说：开机重建是静默跑的，用户看到的只是"目标卷 0/1 就绪"这种
/// **卷级**信息，接不上"到底哪条规则因此没生效"。
/// 而「系统设置模式」的后果更重 —— 环境变量是持久值，变量仍指着那个
/// 不存在的目录，新进程写临时文件会直接失败；链接模式只是链接没恢复。
fn warn_root_not_ready(store: &Store, rule: &Rule, warnings: &mut Vec<String>) {
    let target = engine::resolve(store, rule)
        .map(|r| r.target.display().to_string())
        .ok()
        .or_else(|| {
            store
                .get_root(rule.target_root_id)
                .ok()
                .map(|root| root.resolved_path().0.display().to_string())
        })
        .unwrap_or_default();

    if engine::is_setting_mode(rule) {
        warnings.push(tf("boot.setting_root_not_ready", &[
            ("name", &rule.name),
            ("path", &target),
        ]));
    } else {
        warnings.push(tf("boot.link_root_not_ready", &[
            ("name", &rule.name),
            ("path", &target),
        ]));
    }
}

/// 处理单条规则。返回 `(是否新建了目标目录, 动作)`。
///
/// `warnings` 收"不阻断流程但必须让人看见"的问题（目前只有权限补授权失败）。
fn rebuild_one(
    store: &Store,
    rule: &Rule,
    root: &Root,
    warnings: &mut Vec<String>,
) -> SlResult<(bool, RebuildAction)> {
    let r = engine::resolve(store, rule)?;
    if !r.unknown_vars.is_empty() {
        return Ok((
            false,
            RebuildAction::NeedsAttention(tf("boot.unresolved_vars", &[
                ("vars", &r.unknown_vars.join("、")),
            ])),
        ));
    }
    let (root_path, _) = root.resolved_path();
    if !root_path.exists() {
        return Ok((
            false,
            RebuildAction::NeedsAttention(tf("boot.root_unavailable", &[
                ("path", &root_path.display().to_string()),
            ])),
        ));
    }

    // 「系统设置模式」：环境变量是注册表里的持久值，重启后不会丢。
    // 开机重建只需把 RAM 盘上的目标目录建出来 —— 否则变量会指向一个
    // 不存在的目录，新进程写临时文件直接失败。目录建好即达成目的。
    //
    // 授权不能省：这个目录是本工具建出来的，按权限铁律，
    // 对应使用者必须有完全控制 —— 否则"变量指过来了、程序却写不进去"。
    // （链接模式那条 ensure_dir_access 在下面，这里提前返回所以要自己补。）
    if engine::is_setting_mode(rule) {
        let created = engine::rebuild_setting_dir(store, rule, &r)?;
        // 授权不能省：这个目录是本工具建出来的，按权限铁律，
        // 对应使用者必须有完全控制 —— 否则"变量指过来了、程序却写不进去"。
        // （链接模式那条 ensure_dir_access 在下面，这里提前返回所以要自己补。）
        //
        // 「系统启动时」那一趟以 SYSTEM 身份跑，授的是 SYSTEM 的 ACE；
        // 但目录是**继承** RAM 盘上级目录的 DACL 建出来的，
        // 那里有 `Authenticated Users: Modify` —— 机器级 TEMP 要所有用户可写，
        // 靠的正是这条继承 ACE，不是那个显式的 SYSTEM ACE。
        if let Some(msg) = crate::engine::ensure_dir_access(store, Some(rule.id), &r.target) {
            warnings.push(msg);
        }
        return Ok((created, RebuildAction::AlreadyPresent));
    }

    // 建目录（这一步是开机守护的核心职责）
    let created = ensure_dir(&r.target)?;

    // 需要提权的规则：把首次重定向时导出的 DACL 贴回去
    if let Some(sddl) = rule.acl_sddl.as_deref() {
        if let Err(e) = acl::apply_sddl(&r.target, sddl) {
            store.log_event(
                Some(rule.id),
                "warn",
                "acl_failed",
                &tf("boot.acl_failed", &[("name", &rule.name), ("err", &e.to_string())]),
            )?;
        }
    }

    // 无论目录是谁建的，都要保证"对应使用者"对它有完全控制。
    // 必须放在 SDDL 回灌**之后**：回灌是整份 DACL 覆盖，会把先前加的 ACE 冲掉。
    if let Some(msg) = crate::engine::ensure_dir_access(store, Some(rule.id), &r.target) {
        warnings.push(msg);
    }

    let state = detect_link(&r.source, Some(&r.target));
    match state {
        // 目录刚建好，链接必然有效（目标为空）
        LinkState::LinkOk { .. } => Ok((created, RebuildAction::AlreadyPresent)),
        LinkState::LinkBroken { .. } => {
            // 极少见：目录建好了链接还断，说明链接本身有问题
            let again = detect_link(&r.source, Some(&r.target));
            if again.is_managed_link() {
                Ok((created, RebuildAction::AlreadyPresent))
            } else {
                Ok((created, RebuildAction::NeedsAttention(again.label())))
            }
        }
        LinkState::SourceMissing => {
            // 源完全不存在。父目录还在的话直接把链接建出来 —— 软件装好即可用。
            let parent_ok = r
                .source
                .parent()
                .map(|p| p.as_os_str().is_empty() || p.exists())
                .unwrap_or(false);
            if parent_ok {
                let lt = rule_link_type(rule)?;
                create_dir_link(&r.source, &r.target, lt)?;
                if let Some(msg) = crate::engine::ensure_link_access(store, Some(rule.id), &r.source) {
                    warnings.push(msg);
                }
                Ok((created, RebuildAction::Relinked))
            } else {
                Ok((
                    created,
                    RebuildAction::NeedsAttention(t("boot.source_and_parent_missing").into()),
                ))
            }
        }
        LinkState::PlainDir => Ok((
            created,
            RebuildAction::NeedsAttention(t("boot.source_became_plain").into()),
        )),
        LinkState::ForeignReparse { tag } => Ok((
            created,
            RebuildAction::NeedsAttention(tf("boot.foreign_reparse", &[
                ("tag", &format!("{tag:#010x}")),
            ])),
        )),
        other => Ok((created, RebuildAction::NeedsAttention(other.label()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{apply_rule, ApplyOptions};
    use crate::fsops::mover::Progress;
    use crate::store::{NewRule, Store};
    use std::fs;
    use std::path::PathBuf;

    struct Env {
        base: PathBuf,
        store: Store,
        rule_id: i64,
        source: PathBuf,
        target: PathBuf,
    }

    fn setup(tag: &str) -> Env {
        let base = std::env::temp_dir().join(format!("td_boot_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("cache");
        let ram = base.join("ram");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&ram).unwrap();
        fs::write(source.join("data.bin"), vec![3u8; 512]).unwrap();

        let store = Store::open_in_memory().unwrap();
        let root_id = store
            .insert_root("RAM", &ram.to_string_lossy(), true, 5000)
            .unwrap();
        let rule_id = store
            .insert_rule(&NewRule {
                name: "开机测试".into(),
                category: "dev".into(),
                source_template: source.to_string_lossy().into_owned(),
                target_root_id: root_id,
                target_subpath: "dev\\boot-cache".into(),
                link_type: "junction".into(),
                redirect_mode: "link".into(),
                env_vars: Vec::new(),
                env_scope: "user".into(),
                want_enabled: true,
                rebuild_on_boot: true,
                notes: None,
                excludes: Vec::new(),
                auto_clean_policy: String::new(),
                auto_clean_limit_bytes: 0,
            })
            .unwrap();

        Env {
            base,
            store,
            rule_id,
            source,
            target: ram.join("dev\\boot-cache"),
        }
    }

    #[test]
    fn rebuild_recreates_target_dir_after_ram_loss() {
        let env = setup("ramloss");
        let rule = env.store.get_rule(env.rule_id).unwrap();
        let mut noop = |_: Progress| {};
        apply_rule(&env.store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
        assert!(env.target.exists());

        // 模拟 RAM 盘重启：目标目录整个消失
        fs::remove_dir_all(&env.target).unwrap();

        let report = run_boot_rebuild(&env.store).unwrap();
        assert!(env.target.exists(), "开机重建必须把目标目录建回来");
        assert_eq!(report.dirs_created, 1);
        assert_eq!(report.links_recovered, 1);
        assert_eq!(report.already_ok, 0);
        assert!(!report.has_problem(), "{}", report.one_line());
        // 链接恢复可用
        assert!(matches!(
            detect_link(&env.source, Some(&env.target)),
            LinkState::LinkOk { .. }
        ));

        let _ = fs::remove_dir_all(&env.base);
    }

    #[test]
    fn rebuild_recreates_subfolder_root_after_ram_loss() {
        // 这个用例覆盖的是一条真会踩到的路径：目标根是 RAM 盘**下的子目录**
        // （文档里就推荐 `R:\Redirect` 这种写法），RAM 盘重启后连根目录一起没了。
        // 老实现只"等目录自己出现"，于是永远等不到、整条重建被跳过。
        let base = std::env::temp_dir().join(format!("td_boot_subroot_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("cache");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"x").unwrap();

        // "RAM 盘"用 base\ram2 代表，目标根是它下面的子目录 Redirect
        fs::create_dir_all(base.join("ram2")).unwrap();
        let root_path = base.join("ram2").join("Redirect");

        let store = Store::open_in_memory().unwrap();
        let root_id = store
            .insert_root("RAM", &root_path.to_string_lossy(), true, 3000)
            .unwrap();
        let rule_id = store
            .insert_rule(&NewRule {
                name: "子目录根".into(),
                category: "dev".into(),
                source_template: source.to_string_lossy().into_owned(),
                target_root_id: root_id,
                target_subpath: "dev\\cache".into(),
                link_type: "junction".into(),
                redirect_mode: "link".into(),
                env_vars: Vec::new(),
                env_scope: "user".into(),
                want_enabled: true,
                rebuild_on_boot: true,
                notes: None,
                excludes: Vec::new(),
                auto_clean_policy: String::new(),
                auto_clean_limit_bytes: 0,
            })
            .unwrap();

        // 1) 根目录还不存在 → apply 应该顺手建出来，而不是报"目标根不可用"
        let rule = store.get_rule(rule_id).unwrap();
        let mut noop = |_: Progress| {};
        let out = apply_rule(&store, &rule, &ApplyOptions::default(), &mut noop).unwrap();
        assert_eq!(out.action, "moved_and_linked");
        assert!(root_path.is_dir(), "apply 应把不存在的目标根建出来");

        // 2) 模拟 RAM 盘重启：整个 ram2 消失，然后盘回来、但根目录不在
        fs::remove_dir_all(base.join("ram2")).unwrap();
        fs::create_dir_all(base.join("ram2")).unwrap();

        let report = run_boot_rebuild(&store).unwrap();
        assert!(root_path.is_dir(), "开机重建必须把根目录建出来，不能干等");
        assert!(root_path.join("dev").join("cache").is_dir());
        assert_eq!(report.dirs_created, 1);
        assert!(!report.has_problem(), "{}", report.one_line());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn rebuild_reports_unavailable_root_without_touching_rules() {
        let env = setup("badroot");
        env.store
            .update_root(1, "RAM", "Z:\\NotExistAtAll", true, 1000)
            .unwrap();
        let report = run_boot_rebuild(&env.store).unwrap();
        assert!(report.roots[0].error.is_some());
        assert!(report.has_problem());
        // 源目录原封不动
        assert!(env.source.join("data.bin").exists());
        let _ = fs::remove_dir_all(&env.base);
    }

    #[test]
    fn rebuild_warns_on_plain_dir_instead_of_touching() {
        let env = setup("plain");
        // 源是真实目录（未托管）
        let report = run_boot_rebuild(&env.store).unwrap();
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("真实目录"));
        assert!(env.source.join("data.bin").exists());
        assert!(env.target.exists(), "目标目录仍然应该被建出来");
        let _ = fs::remove_dir_all(&env.base);
    }

    #[test]
    fn rebuild_skips_when_nothing_enabled() {
        let env = setup("disabled");
        env.store.set_rule_enabled(env.rule_id, false).unwrap();
        let report = run_boot_rebuild(&env.store).unwrap();
        assert_eq!(report.rules_total, 0);
        assert!(!env.target.exists());
        let _ = fs::remove_dir_all(&env.base);
    }

    #[test]
    fn scope_splits_protected_and_normal_rules() {
        let env = setup("scope");
        let win = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        env.store
            .insert_rule(&NewRule {
                name: "受保护源".into(),
                category: "system".into(),
                source_template: format!("{win}\\Temp\\td-scope-{}", std::process::id()),
                target_root_id: 1,
                target_subpath: "system\\protected".into(),
                link_type: "junction".into(),
                redirect_mode: "link".into(),
                env_vars: Vec::new(),
                env_scope: "user".into(),
                want_enabled: true,
                rebuild_on_boot: true,
                notes: None,
                excludes: Vec::new(),
                auto_clean_policy: String::new(),
                auto_clean_limit_bytes: 0,
            })
            .unwrap();

        let non_elevated = run_boot_rebuild_scoped(&env.store, BootScope::NonElevatedOnly).unwrap();
        assert_eq!(non_elevated.rules_total, 1, "普通任务只管普通源");

        let elevated = run_boot_rebuild_scoped(&env.store, BootScope::ElevatedOnly).unwrap();
        assert_eq!(elevated.rules_total, 1, "高权限任务只管受保护源");

        let all = run_boot_rebuild_scoped(&env.store, BootScope::All).unwrap();
        assert_eq!(all.rules_total, 2);

        // 两份报告写不同的键，界面才能同时看到
        assert!(env.store.get_setting("last_boot_report").unwrap().is_some());
        assert!(env.store
            .get_setting("last_boot_report_elevated")
            .unwrap()
            .is_some());
        let _ = fs::remove_dir_all(&env.base);
    }

    #[test]
    fn rule_skipped_by_unready_root_is_named_in_warnings() {
        // 目标根没就绪 → 这条规则本轮被跳过。必须**点名**说出来：
        // 旧行为只有卷级错误（"目标卷 0/1 就绪"），用户接不上"哪条规则没生效"。
        let env = setup("skipwarn");
        env.store
            .update_root(1, "RAM", "Z:\\NotExistAtAll", true, 1000)
            .unwrap();
        let report = run_boot_rebuild(&env.store).unwrap();
        assert_eq!(report.rules_total, 0, "根没就绪的规则不进入待处理队列");
        assert!(
            report.warnings.iter().any(|w| w.contains("开机测试")),
            "告警必须点名被跳过的规则：{:?}",
            report.warnings
        );
        assert!(report.has_problem());
        let _ = fs::remove_dir_all(&env.base);
    }

    /// 把一条规则改成「系统设置模式」（其余字段照旧）。
    fn as_setting_rule(rule: &crate::store::Rule) -> NewRule {
        NewRule {
            name: rule.name.clone(),
            category: rule.category.clone(),
            source_template: rule.source_template.clone(),
            target_root_id: rule.target_root_id,
            target_subpath: rule.target_subpath.clone(),
            link_type: rule.link_type.clone(),
            redirect_mode: "setting".into(),
            env_vars: vec!["TEMP".into()],
            env_scope: "user".into(),
            want_enabled: rule.want_enabled,
            rebuild_on_boot: rule.rebuild_on_boot,
            notes: rule.notes.clone(),
            excludes: rule.excludes.clone(),
            auto_clean_policy: rule.auto_clean_policy.clone(),
            auto_clean_limit_bytes: rule.auto_clean_limit_bytes,
        }
    }

    #[test]
    fn setting_only_scope_touches_only_setting_rules() {
        // 「系统启动时」那一趟以 SYSTEM 身份运行：只有系统设置模式规则能进去。
        // 链接模式规则的源路径靠 %LOCALAPPDATA% 之类的变量展开，在 SYSTEM 上下文里
        // 会展开成 systemprofile —— 路径全错，所以必须一条都不碰。
        let env = setup("settingscope");
        let link_rule = env.store.get_rule(env.rule_id).unwrap();
        // 规则库不允许两条规则指向同一个源路径，所以系统设置模式那条要用自己的源。
        let setting_source = env.base.join("setting-cache");
        fs::create_dir_all(&setting_source).unwrap();
        let setting_id = env
            .store
            .insert_rule(&NewRule {
                source_template: setting_source.to_string_lossy().into_owned(),
                target_subpath: "dev\\setting-cache".into(),
                ..as_setting_rule(&link_rule)
            })
            .unwrap();

        let report = run_boot_rebuild_scoped(&env.store, BootScope::SettingOnly).unwrap();
        assert_eq!(report.rules_total, 1, "只该处理那一条系统设置模式规则");

        let setting_target = env.base.join("ram").join("dev").join("setting-cache");
        assert!(
            setting_target.exists(),
            "系统设置模式规则的目标目录必须被建出来 —— 机器级环境变量指着它"
        );
        assert!(
            !env.target.exists(),
            "链接模式规则的目标目录不该被这一趟建出来"
        );

        // 报告写自己的键，界面才能把"启动时那一趟"单独显示出来
        assert!(env.store.get_setting("last_boot_report_startup").unwrap().is_some());
        assert!(env.store.get_setting("last_boot_report").unwrap().is_none());

        let _ = fs::remove_dir_all(&env.base);
        let _ = setting_id;
    }

    #[test]
    fn setting_only_scope_parses_from_flag() {
        assert_eq!(
            BootScope::parse("setting_only"),
            Some(BootScope::SettingOnly)
        );
        assert_eq!(
            BootScope::parse("setting-only"),
            Some(BootScope::SettingOnly)
        );
        assert_eq!(BootScope::parse("nope"), None);
    }

    #[test]
    fn setting_rule_skipped_by_unready_root_warns_about_env_var() {
        // 系统设置模式的后果比链接模式重：变量是持久值，仍指向那个**不存在**的目录，
        // 新进程写临时文件会直接失败。告警必须说到这一层，不能只说"根没就绪"。
        let env = setup("setskipwarn");
        env.store
            .update_root(1, "RAM", "Z:\\NotExistAtAll", true, 1000)
            .unwrap();
        let rule = env.store.get_rule(env.rule_id).unwrap();
        env.store
            .update_rule(env.rule_id, &as_setting_rule(&rule))
            .unwrap();

        let report = run_boot_rebuild(&env.store).unwrap();
        let warned = report
            .warnings
            .iter()
            .find(|w| w.contains("开机测试"))
            .unwrap_or_else(|| panic!("告警必须点名被跳过的规则：{:?}", report.warnings));
        assert!(
            warned.contains("环境变量"),
            "系统设置模式的告警必须点明环境变量仍然指着不存在的目录：{warned}"
        );
        let _ = fs::remove_dir_all(&env.base);
    }

    #[test]
    fn skipped_rule_is_reported_by_its_own_task_only() {
        // 两条登录任务各写各的报告。系统设置模式在开机重建时**不需要提权**
        // （只建目录、不碰注册表），所以这条规则归普通任务管 ——
        // 高权限任务既不该处理它、更不该替它发告警（否则界面上就是"重复计数"）。
        let env = setup("skipwarnscope");
        env.store
            .update_root(1, "RAM", "Z:\\NotExistAtAll", true, 1000)
            .unwrap();
        let rule = env.store.get_rule(env.rule_id).unwrap();
        env.store
            .update_rule(env.rule_id, &as_setting_rule(&rule))
            .unwrap();

        let elevated = run_boot_rebuild_scoped(&env.store, BootScope::ElevatedOnly).unwrap();
        assert!(
            !elevated.warnings.iter().any(|w| w.contains("开机测试")),
            "高权限任务不该替普通任务报这条规则：{:?}",
            elevated.warnings
        );

        let normal = run_boot_rebuild_scoped(&env.store, BootScope::NonElevatedOnly).unwrap();
        assert!(
            normal.warnings.iter().any(|w| w.contains("开机测试")),
            "普通任务必须报自己跳过的规则：{:?}",
            normal.warnings
        );
        let _ = fs::remove_dir_all(&env.base);
    }
}
