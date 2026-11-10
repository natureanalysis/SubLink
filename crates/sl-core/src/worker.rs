//! 提权 worker：把"需要管理员权限"的动作交给一个一次性进程去做。
//!
//! ## 为什么需要它
//!
//! 目录 Junction 本身不需要特权，但**写受保护目录**（`C:\Program Files`、
//! `C:\ProgramData`、`C:\Windows` 下的缓存）需要管理员权限。
//! 让整个 GUI 常驻管理员身份是不可接受的，所以改成"按需提权"：
//! 界面上点一下 → 弹一次 UAC → 一个短命进程干完活就退出。
//!
//! ## 信任模型（这是本模块最要紧的部分）
//!
//! worker 是被提权执行的，所以必须限制它能干什么：
//!
//! 1. **job 文件里只有 rule id，没有任何路径**。
//!    即便有人伪造一份 job，也只能触发"规则库里本来就合法的动作"，
//!    不能凭空让它去删任意目录。
//! 2. **job 必须位于 `<数据目录>\jobs\` 之内**（[`validate_job_location`]）。
//!    这条把"job 从哪来"钉死在我们的数据目录上。
//! 3. worker 里**重跑全部安全校验**（走与 CLI/GUI 完全相同的
//!    `engine::apply_rule` / `engine::restore_rule` 路径，含黑名单与自环检查）。
//! 4. 结果写回 `<job>.result.json`，由调用方读取；worker 自己不做任何 UI。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::{self, ApplyOptions};
use crate::error::{SlError, SlResult};
use crate::fsops::mover::Progress;
use crate::fsops::verify::VerifyMode;
use crate::i18n::tf;
use crate::store::Store;

/// job 文件格式版本。
pub const JOB_VERSION: i32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub schema: i32,
    /// `apply` / `restore` / `boot_rebuild`
    pub kind: String,
    /// 只允许 rule id —— 刻意不含任何路径字段。
    pub rules: Vec<i64>,
    #[serde(default)]
    pub dry_run: bool,
    /// 目标目录非空时是否允许合并。
    #[serde(default)]
    pub allow_merge: bool,
    #[serde(default)]
    pub sha256: bool,
    /// 尽力模式。worker 仍会逐条判断：只对 `safety::requires_elevation`
    /// 为真的源（`C:\Windows` 等受保护前缀）打开，普通目录照旧严格搬运。
    #[serde(default)]
    pub best_effort: bool,
    /// `boot_rebuild` 的分工范围：空 = 全部，`elevated_only` / `non_elevated_only`。
    #[serde(default)]
    pub boot_scope: String,
    /// 发起进程 pid，只用于日志。
    #[serde(default)]
    pub origin_pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobItem {
    pub rule_id: i64,
    pub name: String,
    pub ok: bool,
    pub action: String,
    pub message: String,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub ok: bool,
    pub kind: String,
    pub started_at: String,
    pub finished_at: String,
    pub items: Vec<JobItem>,
    pub error: Option<String>,
}

impl JobResult {
    pub fn one_line(&self) -> String {
        let ok = self.items.iter().filter(|i| i.ok).count();
        let failed = self.items.len() - ok;
        match &self.error {
            Some(e) => tf("worker.failed", &[("err", e)]),
            None => tf("worker.done", &[
                ("ok", &ok.to_string()),
                ("failed", &failed.to_string()),
            ]),
        }
    }
}

pub fn jobs_dir() -> PathBuf {
    crate::paths::jobs_dir()
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 只用于日志可读性，不参与判定；本地时间由调用方格式化
    format!("unix:{secs}")
}

/// 写一份 job 到 `<数据目录>\jobs\`，返回路径。
pub fn write_job(job: &Job) -> SlResult<PathBuf> {
    let dir = jobs_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{}-{}-{stamp}.job.json", job.kind, std::process::id());
    let path = dir.join(name);
    let bytes = serde_json::to_vec_pretty(job)?;
    std::fs::write(&path, bytes)?;
    Ok(path)
}

pub fn result_path_for(job_path: &Path) -> PathBuf {
    let stem = job_path
        .file_name()
        .map(|s| s.to_string_lossy().replace(".job.json", ""))
        .unwrap_or_else(|| "job".into());
    job_path.with_file_name(format!("{stem}.result.json"))
}

pub fn read_result(job_path: &Path) -> Option<JobResult> {
    let p = result_path_for(job_path);
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// 信任锚：job 必须在规范数据目录的 `jobs` 子目录内。
///
/// 这一条挡掉的是"把一个精心构造的 job 塞到随便某个目录、再诱导提权进程去执行它"。
pub fn validate_job_location(path: &Path) -> SlResult<()> {
    let canon = crate::util::normalize_key(&jobs_dir().to_string_lossy());
    // 先绝对化（顺带剥掉 `\\?\` 前缀）再比较 —— 否则 verbatim 路径永远匹配不上，
    // worker 会把自己写的合法 job 全部拒掉。
    let abs = crate::util::simplify(&crate::util::to_verbatim(path));
    let actual = crate::util::normalize_key(&abs.to_string_lossy());
    let prefix = format!("{canon}\\");
    if !actual.starts_with(&prefix) {
        return Err(SlError::SafetyBlocked(tf("worker.bad_location", &[
            ("canon", &canon),
            ("actual", &actual),
        ])));
    }
    Ok(())
}

/// 执行一份 job 并把结果写回磁盘。
pub fn run_job(job_path: &Path) -> SlResult<JobResult> {
    validate_job_location(job_path)?;

    let text = std::fs::read_to_string(job_path)
        .map_err(|e| SlError::Other(tf("worker.read_failed", &[("err", &e.to_string())])))?;
    let job: Job = serde_json::from_str(&text)
        .map_err(|e| SlError::Other(tf("worker.parse_failed", &[("err", &e.to_string())])))?;
    if job.schema > JOB_VERSION {
        return Err(SlError::Other(tf("worker.bad_version", &[
            ("ver", &job.schema.to_string()),
            ("max", &JOB_VERSION.to_string()),
        ])));
    }

    // 时间戳从库里取（本地时间、可读），失败才退回 unix 秒
    let store = Store::open_default()?;
    let started_at = store.now().unwrap_or_else(|_| now_iso());
    let _ = store.log_event(
        None,
        "info",
        "worker_start",
        &tf("worker.start_log", &[
            ("kind", &job.kind),
            ("rules", &format!("{:?}", job.rules)),
            ("pid", &job.origin_pid.to_string()),
        ]),
    );

    let mut items: Vec<JobItem> = Vec::new();
    let mut hard_error: Option<String> = None;

    match job.kind.as_str() {
        "apply" | "restore" => {
            let base_opts = ApplyOptions {
                dry_run: job.dry_run,
                verify: if job.sha256 {
                    VerifyMode::Sha256
                } else {
                    VerifyMode::CountAndSize
                },
                // 提权本身不免除占用检测；只有受保护源在 best_effort 下才放宽
                skip_lock_check: false,
                allow_merge: job.allow_merge,
                best_effort: false,
            };
            for id in &job.rules {
                let rule = match store.get_rule(*id) {
                    Ok(r) => r,
                    Err(e) => {
                        items.push(JobItem {
                            rule_id: *id,
                            name: format!("#{id}"),
                            ok: false,
                            action: "not_found".into(),
                            message: e.to_string(),
                            elapsed_ms: 0,
                        });
                        continue;
                    }
                };
                let mut opts = base_opts.clone();
                if job.best_effort {
                    // 尽力模式只给"确实需要提权"的**源**用：普通目录没理由放弃严格校验。
                    // 用 `source_requires_elevation` 而不是 `rule_requires_elevation`：
                    // 后者还包含"系统设置模式 + 机器级作用域"（写 HKLM 要提权），
                    // 那跟搬数据的严格程度无关 —— 用户级 TEMP 规则即使要提权写注册表，
                    // 它的源目录仍然是普通目录，不该因此放弃占用检测与逐条校验。
                    opts.best_effort = engine::source_requires_elevation(&store, &rule);
                }
                let mut noop = |_: Progress| {};
                let outcome = if job.kind == "apply" {
                    engine::apply_rule(&store, &rule, &opts, &mut noop)
                } else {
                    engine::restore_rule(&store, &rule, &opts, &mut noop)
                };
                match outcome {
                    Ok(o) => items.push(JobItem {
                        rule_id: o.rule_id,
                        name: o.name,
                        ok: true,
                        action: o.action.to_string(),
                        message: o.message,
                        elapsed_ms: o.elapsed_ms,
                    }),
                    Err(e) => items.push(JobItem {
                        rule_id: rule.id,
                        name: rule.name,
                        ok: false,
                        action: "failed".into(),
                        message: e.to_string(),
                        elapsed_ms: 0,
                    }),
                }
            }
        }
        "boot_rebuild" => {
            let scope = match crate::boot::BootScope::parse(&job.boot_scope) {
                Some(s) => s,
                None => {
                    return Err(SlError::InvalidPath(tf("worker.unknown_boot_scope", &[
                        ("scope", &job.boot_scope),
                    ])))
                }
            };
            match crate::boot::run_boot_rebuild_scoped(&store, scope) {
                Ok(report) => items.push(JobItem {
                    rule_id: 0,
                    name: crate::i18n::t("worker.boot_rebuild").into(),
                    ok: !report.has_problem(),
                    action: "boot_rebuild".into(),
                    message: report.one_line(),
                    elapsed_ms: report.elapsed_ms,
                }),
                Err(e) => hard_error = Some(e.to_string()),
            }
        }
        "autostart_elevated_enable" | "autostart_elevated_disable"
        | "autostart_startup_enable" | "autostart_startup_disable" => {
            // exe 与数据目录都不从 job 里取：用 worker 自己的位置和继承来的环境，
            // 这样 job 依旧不需要（也不能）携带任何路径。
            let enable = job.kind.ends_with("_enable");
            let label = if enable {
                crate::i18n::t(match job.kind.as_str() {
                    "autostart_startup_enable" => "worker.autostart_enable_startup",
                    _ => "worker.autostart_enable",
                })
            } else {
                crate::i18n::t(match job.kind.as_str() {
                    "autostart_startup_disable" => "worker.autostart_disable_startup",
                    _ => "worker.autostart_disable",
                })
            };
            let outcome = match job.kind.as_str() {
                "autostart_elevated_enable" => crate::autostart::enable_elevated(
                    &crate::autostart::preferred_exe(),
                    &crate::autostart::extra_args_from_env(),
                ),
                "autostart_elevated_disable" => crate::autostart::disable_elevated(),
                "autostart_startup_enable" => crate::autostart::enable_startup(
                    &crate::autostart::preferred_exe(),
                    &crate::autostart::extra_args_from_env(),
                ),
                _ => crate::autostart::disable_startup(),
            };
            match outcome {
                Ok(msg) => {
                    // 「系统启动时任务」以 SYSTEM 身份运行，普通用户读不到它的实时状态
                    // （schtasks /Query 返回"拒绝访问"）。所以在这里把结果记进规则库，
                    // 让界面与 CLI 在读不到时能如实说明，而不是谎报"未注册"。
                    if job.kind.starts_with("autostart_startup") {
                        let rec = crate::autostart::RecordedTaskState {
                            registered: job.kind == "autostart_startup_enable",
                            command: if job.kind == "autostart_startup_enable" {
                                msg.clone()
                            } else {
                                String::new()
                            },
                            at: store.now().unwrap_or_default(),
                        };
                        if let Ok(json) = serde_json::to_string(&rec) {
                            let _ = store.set_setting(crate::autostart::STARTUP_STATE_KEY, &json);
                        }
                    }
                    items.push(JobItem {
                        rule_id: 0,
                        name: label.into(),
                        ok: true,
                        action: job.kind.clone(),
                        message: msg,
                        elapsed_ms: 0,
                    })
                }
                Err(e) => hard_error = Some(e.to_string()),
            }
        }
        other => {
            return Err(SlError::InvalidPath(tf("worker.unknown_kind", &[
                ("kind", other),
            ])))
        }
    }

    let result = JobResult {
        ok: hard_error.is_none() && items.iter().all(|i| i.ok),
        kind: job.kind.clone(),
        started_at,
        finished_at: store.now().unwrap_or_else(|_| now_iso()),
        items,
        error: hard_error,
    };

    let out = result_path_for(job_path);
    std::fs::write(&out, serde_json::to_vec_pretty(&result)?)?;

    let _ = store.log_event(
        None,
        if result.ok { "info" } else { "warn" },
        "worker_done",
        &result.one_line(),
    );
    Ok(result)
}

/// 运行 job 的入口（供两个二进制的 argv 分支调用）：返回进程退出码。
pub fn run_job_to_exit_code(job_path: &Path) -> i32 {
    match run_job(job_path) {
        Ok(r) => {
            if r.ok {
                0
            } else {
                2
            }
        }
        Err(e) => {
            // 失败也要把结果写出去，让调用方拿到具体原因而不是"什么都没发生"
            let result = JobResult {
                ok: false,
                kind: "unknown".into(),
                started_at: now_iso(),
                finished_at: now_iso(),
                items: Vec::new(),
                error: Some(e.to_string()),
            };
            let _ = std::fs::write(
                result_path_for(job_path),
                serde_json::to_vec_pretty(&result).unwrap_or_default(),
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_job_outside_jobs_dir() {
        let outside = std::env::temp_dir().join("td_evil.job.json");
        let err = validate_job_location(&outside).unwrap_err();
        assert!(matches!(err, SlError::SafetyBlocked(_)), "{err}");
    }

    #[test]
    fn accepts_job_inside_jobs_dir() {
        let inside = jobs_dir().join("apply-1-1.job.json");
        assert!(validate_job_location(&inside).is_ok());
    }

    #[test]
    fn result_path_is_sibling() {
        let p = PathBuf::from(r"C:\data\jobs\apply-1-2.job.json");
        assert_eq!(
            result_path_for(&p),
            PathBuf::from(r"C:\data\jobs\apply-1-2.result.json")
        );
    }

    #[test]
    fn job_has_no_path_fields() {
        // 这条是防回归：job 一旦能携带路径，提权 worker 就变成任意路径操作器。
        let job = Job {
            schema: JOB_VERSION,
            kind: "apply".into(),
            rules: vec![1, 2],
            dry_run: false,
            allow_merge: false,
            sha256: false,
            best_effort: false,
            boot_scope: String::new(),
            origin_pid: 42,
        };
        let json = serde_json::to_string(&job).unwrap();
        for forbidden in ["path", "source", "target", "dir"] {
            assert!(
                !json.to_lowercase().contains(forbidden),
                "job 序列化结果里不应出现 {forbidden}：{json}"
            );
        }
    }
}
