//! 开机自启用例：注册状态查询 / 启用 / 取消 / 立即执行。

use crate::autostart as autostart_svc;
use crate::elevate;
use crate::error::{SlError, SlResult};
use crate::i18n::tf;
use crate::paths;
use crate::store::Store;
use crate::worker;

use super::view::AutostartView;

pub fn autostart_status(store: &Store) -> AutostartView {
    // 「系统启动时任务」以 SYSTEM 身份运行，普通用户读不到它的实时状态 ——
    // 读不到时用上次操作的结果记录如实说明（见 `autostart::status_with_recorded`）。
    let recorded = store
        .get_setting(autostart_svc::STARTUP_STATE_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<autostart_svc::RecordedTaskState>(&s).ok());
    let st = autostart_svc::status_with_recorded(recorded.as_ref());
    AutostartView {
        registered: st.registered,
        task_name: st.task_name,
        command: st.command,
        message: st.message,
        elevated_registered: st.elevated_registered,
        elevated_task_name: st.elevated_task_name,
        elevated_command: st.elevated_command,
        elevated_message: st.elevated_message,
        startup_registered: st.startup_registered,
        startup_task_name: st.startup_task_name,
        startup_command: st.startup_command,
        startup_message: st.startup_message,
        needs_update: st.needs_update,
        elevated: elevate::is_elevated(),
        data_dir: paths::data_dir().display().to_string(),
        extra_args: autostart_svc::extra_args_from_env(),
        recommended_exe: autostart_svc::preferred_exe().display().to_string(),
    }
}

pub fn autostart_enable() -> SlResult<String> {
    let exe = autostart_svc::preferred_exe();
    autostart_svc::enable(&exe, &autostart_svc::extra_args_from_env())
}

pub fn autostart_disable() -> SlResult<String> {
    autostart_svc::disable()
}

pub fn autostart_run_now() -> SlResult<String> {
    autostart_svc::run_now()
}

/// 注册 / 取消"最高权限登录任务"：这两步必须管理员身份，
/// 复用一次性提权 worker（job 里依旧只有种类，没有路径）。
fn run_autostart_job(kind: &str) -> SlResult<String> {
    let job = worker::Job {
        schema: worker::JOB_VERSION,
        kind: kind.to_string(),
        rules: Vec::new(),
        dry_run: false,
        allow_merge: false,
        sha256: false,
        best_effort: false,
        boot_scope: String::new(),
        origin_pid: std::process::id(),
    };
    let job_path = worker::write_job(&job)?;
    let exe = std::env::current_exe()
        .map_err(|e| SlError::Other(tf("elevated.exe_path_failed", &[("err", &e.to_string())])))?;
    let args = elevate::worker_args(&job_path, &autostart_svc::extra_args_from_env());

    let code = elevate::run_elevated_wait(&exe, &args)?;
    let result = worker::read_result(&job_path);
    let _ = std::fs::remove_file(&job_path);
    let _ = std::fs::remove_file(worker::result_path_for(&job_path));

    match result {
        Some(r) if r.ok => Ok(r
            .items
            .first()
            .map(|i| i.message.clone())
            .unwrap_or_default()),
        Some(r) => {
            let summary = r.one_line();
            Err(SlError::Other(r.error.unwrap_or(summary)))
        }
        None => Err(SlError::Other(tf(
            "elevated.no_result",
            &[("code", &code.to_string())],
        ))),
    }
}

/// 注册高权限登录任务（会弹一次 UAC）。
pub fn autostart_elevated_enable() -> SlResult<String> {
    run_autostart_job("autostart_elevated_enable")
}

/// 取消高权限登录任务（会弹一次 UAC）。
pub fn autostart_elevated_disable() -> SlResult<String> {
    run_autostart_job("autostart_elevated_disable")
}

/// 注册「系统启动时」任务（SYSTEM 身份，登录前重建系统设置模式规则的目标目录）。
/// 会弹一次 UAC。
pub fn autostart_startup_enable() -> SlResult<String> {
    run_autostart_job("autostart_startup_enable")
}

/// 取消「系统启动时」任务。会弹一次 UAC。
pub fn autostart_startup_disable() -> SlResult<String> {
    run_autostart_job("autostart_startup_disable")
}
