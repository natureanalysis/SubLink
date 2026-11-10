//! 规则 → 源/目标路径：展开环境变量、收集告警、执行前校验。

use std::path::{Path, PathBuf};

use crate::error::{SlError, SlResult};
use crate::i18n::tf;
use crate::link::detect::LinkType;
use crate::store::{Root, Rule, Store};
use crate::util;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub root: Root,
    pub source: PathBuf,
    pub target: PathBuf,
    pub warnings: Vec<String>,
    pub unknown_vars: Vec<String>,
}

/// 解析一条规则的源与目标（展开环境变量）。
pub fn resolve(store: &Store, rule: &Rule) -> SlResult<Resolved> {
    let root = store.get_root(rule.target_root_id)?;
    let (source, mut unknown) = rule.resolved_source();
    let (target, unk2) = rule.resolved_target(&root);
    unknown.extend(unk2);

    let mut warnings = Vec::new();
    if !unknown.is_empty() {
        warnings.push(tf("resolve.unresolved_vars_warn", &[
            ("vars", &unknown.join("、")),
        ]));
    }
    let pf = crate::safety::preflight_source(&source);
    warnings.extend(pf.warnings.clone());

    Ok(Resolved {
        root,
        source,
        target,
        warnings,
        unknown_vars: unknown,
    })
}

pub fn rule_link_type(rule: &Rule) -> SlResult<LinkType> {
    LinkType::parse(&rule.link_type)
        .ok_or_else(|| {
            SlError::InvalidPath(tf("resolve.unknown_link_type", &[("kind", &rule.link_type)]))
        })
}

/// 这条规则是不是「系统设置模式」（直接改注册表里的环境变量）。
///
/// 两种模式共用同一条流水线（源路径 → 目标根/子目录），但在**落地方式**上
/// 完全不同：链接模式在源路径建 Junction，系统设置模式改环境变量。
pub fn is_setting_mode(rule: &Rule) -> bool {
    rule.redirect_mode == crate::store::REDIRECT_MODE_SETTING
}

/// 这条规则**执行时**（apply / restore）是否需要管理员权限。
///
/// 两个来源，**缺一不可**：
/// 1. 源路径受保护（`C:\Program Files` / `C:\Windows` 等）—— 搬数据要权限；
/// 2. 「系统设置模式」且作用域是机器级（HKLM）—— 写注册表要权限。
///
/// 提权 worker 靠它决定要不要走管理员身份。
pub fn rule_requires_elevation(store: &Store, rule: &Rule) -> bool {
    let scope_needs = is_setting_mode(rule)
        && crate::sysenv::EnvScope::parse(&rule.env_scope)
            .map(|s| s.requires_elevation())
            .unwrap_or(false);
    if scope_needs {
        return true;
    }
    source_requires_elevation(store, rule)
}

/// 这条规则的**源路径**是否受保护（搬数据 / 建链接要管理员权限）。
///
/// 与 [`rule_requires_elevation`] 分开的原因：那个判的是「整体执行要不要提权」，
/// 里面混进了「写 HKLM 要提权」这一条 —— 那跟源路径没关系。
/// 而「尽力模式」与「开机建目录」关心的都是源路径本身，所以要单独一个判定。
pub fn source_requires_elevation(store: &Store, rule: &Rule) -> bool {
    match resolve(store, rule) {
        Ok(r) => crate::safety::requires_elevation(&r.source),
        Err(_) => false,
    }
}

/// 这条规则在**开机重建**时是否需要管理员权限。
///
/// 与 [`rule_requires_elevation`] **刻意不同**：那个判的是「apply / restore
/// 要不要提权」，而「系统设置模式 + 机器级作用域」要提权是因为**写 HKLM**。
/// 但开机重建在 setting 模式下只做两件事：
/// 在 RAM 盘上把目标目录建出来、给使用者授权 —— **不碰注册表**
/// （环境变量的值是持久值，重启后依然在）。
///
/// 所以 setting 模式的规则在开机重建时**永远不需要提权**，必须由普通登录任务处理。
/// 漏掉这一点会让「机器级 TEMP」这类规则在开机后**目标目录不再被重建** ——
/// 高权限登录任务是用户手动才注册的，默认没有；而普通任务又按
/// 「需要提权」把它跳过了，于是 RAM 盘上的目标目录消失，
/// 环境变量指向一个不存在的目录，新进程写临时文件直接失败。
pub fn boot_requires_elevation(store: &Store, rule: &Rule) -> bool {
    if is_setting_mode(rule) {
        return false;
    }
    source_requires_elevation(store, rule)
}

/// 一条规则**真正要清理的目录**：
/// 源已是链接（联接/符号链接）→ 真实内容在 RAM 盘目标上，清目标；
/// 否则（普通目录 / 尚未链接）→ 清源。
///
/// GUI 与 CLI 共用这一条判定，避免"清哪个目录"两处各写一份而漂移。
pub fn clean_dir_for_rule(store: &Store, rule: &Rule) -> SlResult<PathBuf> {
    let resolved = resolve(store, rule)?;
    let source_is_link = std::fs::symlink_metadata(&resolved.source)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    Ok(if source_is_link {
        resolved.target
    } else {
        resolved.source
    })
}

/// 执行前的「环境就绪」校验。
pub(super) fn validate_ready(r: &Resolved) -> SlResult<()> {
    if !r.unknown_vars.is_empty() {
        return Err(SlError::InvalidPath(tf("resolve.unresolved_vars_err", &[
            ("vars", &r.unknown_vars.join("、")),
        ])));
    }
    let (root_path, _) = r.root.resolved_path();
    if !root_path.is_dir() {
        // 目标根可能是 RAM 盘下的子目录。RAM 盘刚重启时"盘在、根目录不在"是正常状态，
        // 直接把它建出来即可 —— 不该让用户先去点一次「开机重建」才能应用规则。
        crate::boot::ensure_root_dir(&root_path)?;
    }
    crate::safety::validate_rule(&r.source, &r.target)?;
    Ok(())
}

pub(super) fn dir_has_content(dir: &Path) -> bool {
    std::fs::read_dir(util::to_verbatim(dir))
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}