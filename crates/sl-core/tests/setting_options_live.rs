//! 「系统设置模式」可行性的**真机**判定验证。
//!
//! 单元测试（`engine::setting_options_tests`）只覆盖纯比较逻辑；这条集成测试
//! 补上"读真注册表"那一层：判定依据是注册表里 `TEMP` / `TMP` 的**实际值**，
//! 所以只有在这台机器上跑才算真的验过。
//!
//! ## 钉的三件事
//!
//! 1. 本机用户级 `TEMP` 指向的目录 → 有可行组合（作用域 user、变量含 TEMP）；
//! 2. 随便一个没人指向的目录（`C:\Windows\Debug` 是典型的"写死的目录路径"）
//!    → 没有可行组合 —— 这正是界面不给出「系统设置」选项的那条规则；
//! 3. 判定结果与 `check_source_matches` 的语义一致：读不到的变量不参与判定。

use std::path::PathBuf;

use sl_core::engine::setting_options_for_source;
use sl_core::sysenv::{self, EnvScope};

/// 本机用户级 TEMP 指向的目录应当有可行组合。
///
/// 这台机器上注册表里读不到用户级 TEMP 时跳过（服务账户等少数环境如此）——
/// 那种环境本来就无从判定，不该让测试红。
#[test]
fn user_temp_directory_has_a_feasible_combination() {
    let Ok(Some(cur)) = sysenv::read_env_expanded(EnvScope::User, "TEMP") else {
        eprintln!("跳过：这台机器读不到用户级 TEMP");
        return;
    };
    let cur = cur.trim();
    if cur.is_empty() {
        eprintln!("跳过：用户级 TEMP 是空值");
        return;
    }
    let opts = setting_options_for_source(&PathBuf::from(cur));
    assert!(
        !opts.is_empty(),
        "用户级 TEMP 指向的目录 {cur} 应当有可行组合"
    );
    let user = opts
        .iter()
        .find(|o| o.scope == "user")
        .expect("应当有用户级的组合");
    assert!(
        user.vars.iter().any(|v| v == "TEMP"),
        "用户级组合应当包含 TEMP，实得 {:?}",
        user.vars
    );
}

/// 写死的目录路径（不是任何变量指向的目录）不该有可行组合。
///
/// `C:\Windows\Debug` 就是本机的真实例子：它是 Windows 写死的路径，
/// 没有任何环境变量指向它 —— 所以「系统设置」模式对它不可实现。
#[test]
fn hardcoded_directory_has_no_feasible_combination() {
    for path in [r"C:\Windows\Debug", r"C:\Windows\Logs", r"C:\Windows\System32"] {
        let opts = setting_options_for_source(&PathBuf::from(path));
        assert!(
            opts.is_empty(),
            "{path} 没有任何变量指向，不该有可行组合，实得 {opts:?}"
        );
    }
}
