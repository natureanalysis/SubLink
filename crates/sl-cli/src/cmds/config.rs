//! 配置迁移：export / import。
//!
//! export 走 `usecase::export_config`（与 GUI 同一条路径）；
//! import 保留对 `portable` 的直接调用 —— CLI 在导入前要检查 `bundle.app`
//! 并对非本程序导出的文件打印警告，这个细节在 usecase 门面之外。

use sl_core::i18n::{t, tf};
use sl_core::portable;
use sl_core::store::Store;
use sl_core::usecase;
use sl_core::{SlError, SlResult};

use crate::parse::Parsed;

pub(crate) fn cmd_export(store: &Store, p: &Parsed) -> SlResult<()> {
    let path = p.required(0, "<输出文件.json>")?;
    let (roots, rules) = usecase::export_config(store, std::path::Path::new(&path))?;
        println!("{}", tf("cli.config.exported", &[("roots", &roots.to_string()), ("rules", &rules.to_string()), ("path", path.as_str())]));
        println!("{}", t("cli.config.export_hint"));
    Ok(())
}

pub(crate) fn cmd_import(store: &Store, p: &Parsed) -> SlResult<()> {
    let path = p.required(0, "<输入文件.json>")?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| SlError::Other(format!("读取 {path} 失败：{e}")))?;
    let bundle: portable::Bundle = serde_json::from_str(&text)
        .map_err(|e| SlError::Other(format!("解析 {path} 失败：{e}")))?;

    if !portable::is_known_app(&bundle.app) {
        println!("{}", tf("cli.config.import_warn_app", &[("app", bundle.app.as_str())]));
    }
    let report = portable::import_bundle(store, &bundle, p.value("ram-root"), p.has("dry-run"))?;
    println!("{}", report.one_line());
    for e in &report.errors {
        println!("  ⚠ {e}");
    }
    Ok(())
}