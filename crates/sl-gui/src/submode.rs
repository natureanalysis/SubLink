//! 无界面子模式：由命令行参数触发，**不启动 Tauri**。
//!
//! 两种模式都必须在 `tauri::Builder` 之前拦下来：
//!
//! * `boot-rebuild` —— 登录任务调它。release 构建的 GUI 是
//!   `windows_subsystem = "windows"`，被任务计划拉起时**不闪控制台窗口**；
//!   CLI 是控制台程序，适合人手动跑。两者共用 `sl_core::boot` 里的同一份实现。
//! * `elevated <job>` —— 提权 worker。用"自己"去提权还有个额外好处：
//!   进程名始终是 SubLink，UAC 弹窗上显示的就是用户认识的那个程序，
//!   而且 windowed 子系统不会有控制台闪一下。
//!
//! 结果不靠 stdout 传递：登录任务是静默跑的，所以重建报告会落库
//! （普通任务写 `settings.last_boot_report`，高权限任务写
//! `settings.last_boot_report_elevated`，系统启动时任务写
//! `settings.last_boot_report_startup`），界面下次启动时在顶部提示。

use std::path::{Path, PathBuf};

use sl_core::paths;

/// 子模式也支持 `--data-dir`（便携模式 / 自动化测试），必须在开库之前生效。
const DATA_DIR_FLAG: &str = "--data-dir";

/// 处理子模式。返回 `Some(exit_code)` 表示"不要启动界面"。
pub fn dispatch(args: &[String]) -> Option<i32> {
    let cmd = args.first()?.as_str();
    if !matches!(cmd, "boot-rebuild" | "elevated" | "help" | "--help" | "-h") {
        // 其它参数交给 Tauri（将来可能要支持"用本程序打开某个配置文件"）
        return None;
    }

    apply_data_dir_override(args);

    match cmd {
        "boot-rebuild" => Some(run_boot_rebuild(args)),
        "elevated" => {
            let job = args.get(1).map(String::as_str).unwrap_or("");
            if job.is_empty() {
                eprintln!("用法：sublink-gui elevated <job.json>");
                return Some(1);
            }
            Some(sl_core::worker::run_job_to_exit_code(Path::new(job)))
        }
        _ => {
            print_usage();
            Some(0)
        }
    }
}

fn apply_data_dir_override(args: &[String]) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == DATA_DIR_FLAG {
            if let Some(v) = args.get(i + 1) {
                std::env::set_var(paths::DATA_DIR_ENV, v);
            }
        } else if let Some(v) = args[i].strip_prefix("--data-dir=") {
            std::env::set_var(paths::DATA_DIR_ENV, v);
        }
        i += 1;
    }
}

/// 静默执行一次开机重建。退出码：0 正常 / 2 有告警或失败 / 1 致命。
///
/// `--elevated-only` / `--skip-elevated` 由两个登录任务分别传入，
/// 让普通权限任务别再为受保护目录报一堆"无权限"；
/// `--setting-only` 由**系统启动时**任务传入（SYSTEM 身份、登录前跑），
/// 只重建系统设置模式规则的目标目录 —— 机器级环境变量指着它，
/// 目录不存在的话，开机自启服务写临时文件直接失败。
fn run_boot_rebuild(args: &[String]) -> i32 {
    let scope = if args.iter().any(|a| a == "--elevated-only") {
        sl_core::boot::BootScope::ElevatedOnly
    } else if args.iter().any(|a| a == "--skip-elevated") {
        sl_core::boot::BootScope::NonElevatedOnly
    } else if args.iter().any(|a| a == "--setting-only") {
        sl_core::boot::BootScope::SettingOnly
    } else {
        sl_core::boot::BootScope::All
    };
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("创建数据目录失败：{e}");
        return 1;
    }
    let store = match sl_core::store::Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("打开规则库失败：{e}");
            return 1;
        }
    };
    // 上次崩溃留下的"进行中"流水先收尾，否则会一直挂在待修复列表里
    let _ = store.mark_stale_ops();

    match sl_core::boot::run_boot_rebuild_scoped(&store, scope) {
        Ok(report) => {
            // 无控制台也要留个痕迹：写一份纯文本日志到 logs/
            let line = format!(
                "[{}] {}\n",
                store.now().unwrap_or_default(),
                report.one_line()
            );
            let _ = std::fs::create_dir_all(paths::log_dir());
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(paths::log_dir().join("boot-rebuild.log"))
            {
                use std::io::Write;
                let _ = f.write_all(line.as_bytes());
            }
            if report.has_problem() {
                eprintln!("{}", report.one_line());
                2
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("开机重建失败：{e}");
            1
        }
    }
}

fn print_usage() {
    println!("SubLink —— 缓存目录重定向");
    println!();
    println!("直接运行不带参数会打开界面。");
    println!();
    println!("无界面模式：");
    println!(
        "  sublink-gui boot-rebuild [--elevated-only|--skip-elevated|--setting-only] [--data-dir <路径>]"
    );
    println!("                                                       静默重建目标目录（登录任务用）");
    println!("      --elevated-only  只处理 C:\\Windows 等受保护源（高权限任务）");
    println!("      --skip-elevated  只处理普通源（普通权限任务）");
    println!("      --setting-only   只处理系统设置模式规则（系统启动时任务，SYSTEM 身份）");
    println!("  sublink-gui elevated <job.json>                 以管理员身份执行一份任务");
    println!();
    println!("日常命令行操作请用 sublink.exe（见其 help）。");
}

/// 给任务计划用的参数尾巴：便携模式（数据目录来自环境变量）时，
/// 必须把数据目录写进任务，否则登录任务会用默认目录、和用户看到的规则库对不上。
pub fn extra_args_for_current_env() -> String {
    sl_core::autostart::extra_args_from_env()
}

/// 注册开机自启时应该用哪个 exe：优先同目录下的 GUI（静默），其次自己。
pub fn preferred_exe() -> PathBuf {
    sl_core::autostart::preferred_exe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_submode_returns_none() {
        assert!(dispatch(&["whatever-flag".to_string()]).is_none());
        assert!(dispatch(&[]).is_none());
    }

    #[test]
    fn help_is_a_submode() {
        assert_eq!(dispatch(&["help".to_string()]), Some(0));
    }

    #[test]
    fn elevated_without_job_is_an_error() {
        assert_eq!(dispatch(&["elevated".to_string()]), Some(1));
    }

    #[test]
    fn extra_args_only_when_env_override_is_set() {
        // 这条测试不改全局环境（会影响并行测试），只验证"不带覆盖时必须为空"
        if std::env::var(paths::DATA_DIR_ENV).is_err() {
            assert_eq!(extra_args_for_current_env(), "");
        }
    }
}
