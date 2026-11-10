//! SubLink 命令行工具：入口与命令分发。
//!
//! 结构（见各模块）：
//! * [`parse`] —— 极简参数解析（不引 clap）
//! * [`output`] —— 控制台输出格式化
//! * [`timefmt`] —— 时间参数解析与相对时长
//! * [`cmds`] —— 各子命令实现
//!
//! 历史角色说明：`elevated` 子命令是提权 worker 入口（GUI 以 runas 拉起本程序），
//! 见 `docs/ARCHITECTURE.md` 的提权设计。

use std::process::ExitCode;

use sl_core::i18n::{set_locale, t, tf, Locale};
use sl_core::paths;
use sl_core::store::Store;
use sl_core::{SlError, SlResult};

mod cmds;
mod help;
mod output;
mod parse;
mod timefmt;

use parse::Parsed;

fn run() -> SlResult<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        help::print_help();
        return Ok(());
    }

    // `--data-dir` 和 `--lang` 允许放在任何位置：先扫一遍再决定数据根与语言。
    let mut lang_seen = false;
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--data-dir" {
            if let Some(v) = argv.get(i + 1) {
                std::env::set_var(paths::DATA_DIR_ENV, v);
            }
        } else if let Some(v) = argv[i].strip_prefix("--data-dir=") {
            std::env::set_var(paths::DATA_DIR_ENV, v);
        } else if argv[i] == "--lang" {
            if let Some(v) = argv.get(i + 1) {
                if let Some(lc) = Locale::parse(v) {
                    set_locale(lc);
                    lang_seen = true;
                }
            }
        } else if let Some(v) = argv[i].strip_prefix("--lang=") {
            if let Some(lc) = Locale::parse(v) {
                set_locale(lc);
                lang_seen = true;
            }
        }
        i += 1;
    }

    // 环境变量 SUBLINK_LANG 优先级低于 --lang，仅当未指定 --lang 时回退；
    // 更名前使用的 TEMPDIRECTOR_LANG 仍然认，避免已有脚本失效。
    if !lang_seen {
        for key in ["SUBLINK_LANG", "TEMPDIRECTOR_LANG"] {
            if let Ok(v) = std::env::var(key) {
                if let Some(lc) = Locale::parse(&v) {
                    set_locale(lc);
                    break;
                }
            }
        }
    }

    let cmd = argv[0].as_str();
    if cmd == "help" || cmd == "-h" || cmd == "--help" {
        help::print_help();
        return Ok(());
    }
    if cmd == "version" || cmd == "--version" || cmd == "-V" {
        return cmds::status::cmd_version();
    }

    // 提权 worker 模式：必须在打开数据库之前处理 —— 退出码要精确
    // （0 成功 / 2 部分失败 / 1 致命），而且它自己开库、自己写结果文件。
    if cmd == "elevated" {
        let job = argv
            .get(1)
            .ok_or_else(|| SlError::Other(t("cli.elevated_usage").into()))?;
        let code = sl_core::worker::run_job_to_exit_code(std::path::Path::new(job));
        std::process::exit(code);
    }

    paths::ensure_dirs()
        .map_err(|e| SlError::Other(tf("cli.mkdir_failed", &[("err", &e.to_string())])))?;
    let store = Store::open_default()?;

    let rest: Vec<String> = argv[1..].to_vec();
    let p = Parsed::parse(&rest);

    match cmd {
        "doctor" => cmds::status::cmd_doctor(&store),
        "roots" => cmds::roots::cmd_roots(&store, &p),
        "rules" => cmds::rules::cmd_rules(&store, &p),
        "status" => cmds::status::cmd_status(&store, &p),
        "locks" => cmds::status::cmd_locks(&store, &p),
        "clean" => cmds::clean::cmd_clean(&store, &p),
        "apply" => cmds::apply::cmd_apply(&store, &p),
        "restore" => cmds::apply::cmd_restore(&store, &p),
        "relink" => cmds::apply::cmd_relink(&store, &p),
        "boot-rebuild" => cmds::boot::cmd_boot_rebuild(&store, &p),
        "acl" => cmds::acl::cmd_acl(&store, &p),
        "volume" => cmds::status::cmd_volume(&p),
        "water" => cmds::status::cmd_water(&store, &p),
        "autostart" => cmds::boot::cmd_autostart(&store, &p),
        "export" => cmds::config::cmd_export(&store, &p),
        "import" => cmds::config::cmd_import(&store, &p),
        "events" => cmds::status::cmd_events(&store, &p),
        "ops" => cmds::status::cmd_ops(&store, &p),
        "stats" => cmds::status::cmd_stats(&store, &p),
        other => {
            help::print_help();
            Err(SlError::Other(tf("cli.unknown_cmd", &[("cmd", other)])))
        }
    }
}

fn main() -> ExitCode {
    // 让中文在 Windows 控制台正常显示
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
    }

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!();
            eprintln!("{}", tf("cli.error_prefix", &[("err", &e.to_string())]));
            ExitCode::FAILURE
        }
    }
}