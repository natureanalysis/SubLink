//! 提权用例：把 apply/restore 交给一次性管理员进程执行，再读回结果。

use crate::elevate;
use crate::error::{SlError, SlResult};
use crate::i18n::{t, tf};
use crate::store::Store;
use crate::worker;

use super::view::{ApplyOpts, ItemOutcome};

/// 通过提权 worker 执行 apply / restore。
///
/// 父进程**不开新连接也不加锁**：worker 是独立进程、自己开库（WAL 下多连接没问题），
/// 干完写一份 result.json，这里读回来转成界面用的结果。
pub fn run_elevated_job(
    store: &Store,
    kind: &str,
    ids: &[i64],
    opts: &ApplyOpts,
) -> SlResult<Vec<ItemOutcome>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if opts.dry_run {
        return Err(SlError::Other(t("elevated.dry_run_no_elev").into()));
    }

    let job = worker::Job {
        schema: worker::JOB_VERSION,
        kind: kind.to_string(),
        rules: ids.to_vec(),
        dry_run: false,
        allow_merge: opts.allow_merge,
        sha256: opts.sha256,
        best_effort: opts.best_effort,
        boot_scope: String::new(),
        origin_pid: std::process::id(),
    };
    let job_path = worker::write_job(&job)?;
    let exe = std::env::current_exe()
        .map_err(|e| SlError::Other(tf("elevated.exe_path_failed", &[("err", &e.to_string())])))?;
    let args = elevate::worker_args(&job_path, &crate::autostart::extra_args_from_env());

    let code = elevate::run_elevated_wait(&exe, &args)?;
    let result = worker::read_result(&job_path);
    // 清理：job 与结果文件都是一次性的
    let _ = std::fs::remove_file(&job_path);
    let _ = std::fs::remove_file(worker::result_path_for(&job_path));

    match result {
        Some(r) => {
            store.log_event(
                None,
                if r.ok { "info" } else { "warn" },
                "elevated_job",
                &tf(
                    "elevated.job_log",
                    &[("kind", kind), ("summary", &r.one_line())],
                ),
            )?;
            Ok(r.items
                .into_iter()
                .map(|i| ItemOutcome {
                    rule_id: i.rule_id,
                    name: i.name,
                    ok: i.ok,
                    action: i.action,
                    message: i.message,
                    elapsed_ms: i.elapsed_ms,
                })
                .collect())
        }
        None => Err(SlError::Other(tf(
            "elevated.no_result",
            &[("code", &code.to_string())],
        ))),
    }
}
