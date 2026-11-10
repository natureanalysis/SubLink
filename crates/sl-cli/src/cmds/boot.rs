//! 开机重建与登录任务：boot-rebuild / autostart。

use std::path::PathBuf;

use sl_core::boot::BootScope;
use sl_core::i18n::{t, tf};
use sl_core::usecase;
use sl_core::{SlError, SlResult};

use crate::parse::Parsed;

fn boot_scope(p: &Parsed) -> BootScope {
    if p.has("elevated-only") {
        BootScope::ElevatedOnly
    } else if p.has("skip-elevated") {
        BootScope::NonElevatedOnly
    } else if p.has("setting-only") {
        // 「系统启动时」那一趟：只重建系统设置模式规则的目标目录。
        BootScope::SettingOnly
    } else {
        BootScope::All
    }
}

pub(crate) fn cmd_boot_rebuild(store: &sl_core::store::Store, p: &Parsed) -> SlResult<()> {
    println!("{}", t("cli.boot.start"));
    let report = usecase::boot_rebuild_scoped(store, boot_scope(p))?;
    if p.json() {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    for r in &report.roots {
        let icon = if r.ready { "✅" } else { "❌" };
        let detail = if r.ready {
            tf("cli.boot.waited", &[("ms", &r.waited_ms.to_string())])
        } else {
            t("cli.boot.not_ready").to_string()
        };
        let err = r
            .error
            .as_ref()
            .map(|e| format!("\n      {e}"))
            .unwrap_or_default();
        println!(
            "{}",
            tf(
                "cli.boot.root_row",
                &[
                    ("icon", icon),
                    ("id", &r.root_id.to_string()),
                    ("path", r.path.as_str()),
                    ("label", r.label.as_str()),
                    ("detail", detail.as_str()),
                    ("err", err.as_str()),
                ],
            ),
        );
    }
    println!();
    println!("{}", report.one_line());
    for w in &report.warnings {
        println!("  ⚠ {w}");
    }
    for (id, e) in &report.failed {
        println!("  ❌ #{id} {e}");
    }
    if report.has_problem() {
        return Err(SlError::Other("开机重建存在未就绪项".into()));
    }
    Ok(())
}

pub(crate) fn cmd_autostart(store: &sl_core::store::Store, p: &Parsed) -> SlResult<()> {
    use sl_core::autostart;
    match p.pos(0).unwrap_or("status") {
        "status" => {
            let st = usecase::autostart_status(store);
            let icon = if st.registered {
                t("cli.boot.autostart_registered")
            } else {
                t("cli.boot.autostart_unregistered")
            };
            println!(
                "{}",
                tf("cli.boot.autostart_status", &[("icon", icon), ("name", st.task_name.as_str())])
            );
            println!("   {}", st.message);
            if st.registered {
                println!("{}", t("cli.boot.trigger"));
                println!("{}", t("cli.boot.note"));
            }
            let e_icon = if st.elevated_registered {
                t("cli.boot.autostart_registered")
            } else {
                t("cli.boot.autostart_unregistered")
            };
            println!(
                "{}",
                tf("cli.boot.autostart_status", &[
                    ("icon", e_icon),
                    ("name", st.elevated_task_name.as_str()),
                ])
            );
            println!("   {}", st.elevated_message);
            if st.elevated_registered {
                println!("{}", t("cli.boot.trigger_elevated"));
            }
            let s_icon = if st.startup_registered {
                t("cli.boot.autostart_registered")
            } else {
                t("cli.boot.autostart_unregistered")
            };
            println!(
                "{}",
                tf("cli.boot.autostart_status", &[
                    ("icon", s_icon),
                    ("name", st.startup_task_name.as_str()),
                ])
            );
            println!("   {}", st.startup_message);
            if st.startup_registered {
                println!("{}", t("cli.boot.trigger_startup"));
            }
            if st.needs_update {
                println!("{}", t("cli.boot.task_needs_update"));
            }
            Ok(())
        }
        "enable" => {
            if p.has("startup") {
                // 「系统启动时」任务：SYSTEM 身份 + 启动触发器，注册要管理员权限。
                println!("{}", usecase::autostart_startup_enable()?);
                println!("{}", t("cli.boot.startup_hint"));
                return Ok(());
            }
            if p.has("elevated") {
                println!("{}", usecase::autostart_elevated_enable()?);
                println!("{}", t("cli.boot.elevated_hint"));
                return Ok(());
            }
            // `--exe` 允许指定别的执行体（默认推荐 GUI 静默二进制），
            // 所以不走 usecase::autostart_enable 的固定版本。
            let exe = match p.value("exe") {
                Some(v) => PathBuf::from(v),
                None => autostart::preferred_exe(),
            };
            let cmd = autostart::enable(&exe, &autostart::extra_args_from_env())?;
            println!("{}", t("cli.boot.enabled"));
            println!(
                "{}",
                tf("cli.boot.task_name", &[("name", autostart::TASK_NAME)])
            );
            println!("{}", tf("cli.boot.cmd_line", &[("cmd", cmd.as_str())]));
            println!("{}", t("cli.boot.verify_hint"));
            Ok(())
        }
        "disable" => {
            if p.has("startup") {
                println!("{}", usecase::autostart_startup_disable()?);
                return Ok(());
            }
            if p.has("elevated") {
                println!("{}", usecase::autostart_elevated_disable()?);
                return Ok(());
            }
            println!("{}", usecase::autostart_disable()?);
            Ok(())
        }
        "run" => {
            println!("{}", usecase::autostart_run_now()?);
            println!("{}", t("cli.boot.run_hint"));
            Ok(())
        }
        other => Err(SlError::Other(format!(
            "未知的 autostart 子命令：{other}（可用：status / enable / disable / run）"
        ))),
    }
}