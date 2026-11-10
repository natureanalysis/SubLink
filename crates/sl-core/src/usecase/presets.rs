//! 内置预设（常用软件的缓存目录，M5 预设规则库的第一步）。

use std::path::{Path, PathBuf};

use crate::error::{SlError, SlResult};
use crate::i18n::{t, tf};
use crate::paths;
use crate::store::CATEGORIES;
use crate::util;

use super::view::{PresetDef, PresetView};

/// 内置预设随二进制发布：改这个文件不需要动 Rust 代码，但要重新编译。
/// 装机后的扩展走用户外置文件（见 [`user_presets_path`]），无需重编译。
pub(crate) const PRESETS_JSON: &str = include_str!("../../presets.json");

/// 用户外置预设的位置：`%PROGRAMDATA%\SubLink\presets.json`。
///
/// 格式与内置文件完全一致（JSON 数组）。**追加**新预设；与内置条目
/// `source_template` 相同（忽略大小写）的会**覆盖**内置条目——比如想给
/// 「用户 TEMP」换个子路径，不用改程序。文件不存在=只有内置预设；
/// 文件有语法错误或个别条目非法=跳过那些条目，**绝不让整个预设列表失效**。
pub fn user_presets_path() -> PathBuf {
    paths::data_dir().join("presets.json")
}

/// 解析内置预设。文件随仓库走且有测试钉住（`builtin_presets_parse`），
/// 走到 panic 说明发布流程出了问题，宁可大声失败也不静默缺预设。
pub(crate) fn builtin_presets() -> &'static Vec<PresetDef> {
    static CACHE: std::sync::OnceLock<Vec<PresetDef>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        serde_json::from_str(PRESETS_JSON)
            .expect("内嵌 presets.json 解析失败——仓库文件坏了，修好再发布")
    })
}

/// 读取用户外置预设。返回 `(有效条目, 跳过原因列表)`。
/// 逐条校验（分类合法、字段非空），坏条目跳过而不是整个文件报废。
pub(crate) fn load_user_presets(path: &Path) -> (Vec<PresetDef>, Vec<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return (Vec::new(), Vec::new()), // 没有=正常
    };
    let parsed: Vec<PresetDef> = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return (
                Vec::new(),
                vec![tf("preset.parse_failed", &[("err", &e.to_string())])],
            )
        }
    };
    let mut ok = Vec::new();
    let mut skipped = Vec::new();
    for p in parsed {
        match validate_preset(&p) {
            Ok(()) => ok.push(p),
            Err(e) => skipped.push(tf(
                "preset.skipped",
                &[("name", &p.name), ("err", &e.to_string())],
            )),
        }
    }
    (ok, skipped)
}

pub(crate) fn validate_preset(p: &PresetDef) -> SlResult<()> {
    if p.name.trim().is_empty() {
        return Err(SlError::InvalidPath(t("form.name_empty").into()));
    }
    if !CATEGORIES.contains(&p.category.as_str()) {
        return Err(SlError::InvalidPath(tf(
            "form.unknown_category",
            &[("cat", &p.category), ("list", &CATEGORIES.join(" / "))],
        )));
    }
    if p.source_template.trim().is_empty() {
        return Err(SlError::InvalidPath(t("form.source_empty").into()));
    }
    if p.target_subpath.trim().is_empty() || p.target_subpath.contains(':') {
        return Err(SlError::InvalidPath(
            t("form.preset_subpath_relative").into(),
        ));
    }
    // 「系统设置模式」的预设：变量名必须在白名单内，作用域必须合法。
    if p.redirect_mode == crate::store::REDIRECT_MODE_SETTING {
        crate::sysenv::normalize_vars(&p.env_vars)?;
        if crate::sysenv::EnvScope::parse(&p.env_scope).is_none() {
            return Err(SlError::InvalidPath(t("form.bad_env_scope").into()));
        }
    }
    Ok(())
}

/// 内置 + 用户外置合并。用户条目按 `source_template`（忽略大小写）覆盖同源内置条目，
/// 其余追加在后。内置条目本身若有重复也会在测试里被钉住（`builtin_presets_parse`）。
pub(crate) fn merge_presets(builtin: &[PresetDef], user: &[PresetDef]) -> Vec<PresetDef> {
    let mut out: Vec<PresetDef> = builtin.to_vec();
    for u in user {
        let key = u.source_template.to_lowercase();
        if let Some(b) = out
            .iter_mut()
            .find(|b| b.source_template.to_lowercase() == key)
        {
            *b = u.clone();
        } else {
            out.push(u.clone());
        }
    }
    out
}

/// 全量预设（内置 + 用户外置合并后）。跳过的坏条目原因打到 stderr，
/// 桌面端没有控制台也不影响主流程。
fn all_presets() -> Vec<PresetDef> {
    let user_path = user_presets_path();
    let (user, skipped) = load_user_presets(&user_path);
    for s in skipped {
        eprintln!("{}", tf("preset.warn_prefix", &[("msg", &s)]));
    }
    merge_presets(builtin_presets(), &user)
}

/// 返回内置预设，**只含本机实际存在源目录的条目**（软件没装的自动隐掉）。
/// 逐条做 `is_dir` 探测，量级只有二十来个，无需缓存。
pub fn preset_rules() -> Vec<PresetView> {
    all_presets()
        .into_iter()
        .map(|p| {
            let (expanded, unknown) = util::expand_path(&p.source_template);
            PresetView {
                name: p.name,
                category: p.category,
                source_template: p.source_template,
                target_subpath: p.target_subpath,
                note: p.note,
                source: expanded.display().to_string(),
                exists: unknown.is_empty() && expanded.is_dir(),
                redirect_mode: p.redirect_mode,
                env_vars: p.env_vars,
                env_scope: p.env_scope,
                setting_options: crate::engine::setting_options_for_source(&expanded),
            }
        })
        .filter(|v| v.exists)
        .collect()
}
