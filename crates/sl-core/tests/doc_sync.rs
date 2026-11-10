//! 文档同步断言：`lib.rs` 顶部分层表、`docs/ARCHITECTURE.md` §1 三圈分工
//! 必须与实际的 `pub mod` 声明一致。新增 / 重命名 / 删除模块时若忘了同步文档，
//! 这里会直接红掉并指出差异。

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn repo_file(rel: &[&str]) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // 仓库根
    for seg in rel {
        p.push(seg);
    }
    p
}

fn is_module_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `pub mod X;` 声明的模块集合（唯一事实来源）。
fn declared_modules(lib_src: &str) -> BTreeSet<&str> {
    lib_src
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("pub mod ")
                .and_then(|r| r.strip_suffix(';'))
        })
        .collect()
}

/// lib.rs 顶部分层表里 `[`X`]` 形式提到的模块集合。
fn doc_table_modules(lib_src: &str) -> BTreeSet<&str> {
    lib_src
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            l.strip_prefix("//! | [`")
                .and_then(|r| r.split('`').next())
        })
        .collect()
}

/// ARCHITECTURE.md §1「三圈分工」段落里反引号提到的模块集合。
/// `link::create` 只取 `link`；`sl-core`、`GUI/CLI` 这类非模块标识符的
/// token 会被 [`is_module_name`] 过滤掉。
fn architecture_modules(arch_src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut in_layering = false;
    for line in arch_src.lines() {
        if !in_layering {
            if line.contains("用例门面") {
                in_layering = true;
            } else {
                continue;
            }
        }
        // 段落由缩进行构成（子列表项与续行都带缩进）；遇到顶格行或空行即结束。
        if !line.starts_with(' ') {
            break;
        }
        for tok in line.split('`') {
            let first = tok.split("::").next().unwrap_or(tok);
            if is_module_name(first) && first != "td" {
                out.insert(first.to_string());
            }
        }
    }
    out
}

#[test]
fn lib_doc_table_matches_pub_mods() {
    let path = repo_file(&["crates", "sl-core", "src", "lib.rs"]);
    let src = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读 {}: {e}", path.display()));

    let declared = declared_modules(&src);
    let documented = doc_table_modules(&src);


    let only_doc: Vec<_> = documented.difference(&declared).collect();
    let only_code: Vec<_> = declared.difference(&documented).collect();
    assert!(
        only_doc.is_empty() && only_code.is_empty(),
        "lib.rs 顶部分层表与 pub mod 声明不一致：\n  只在文档表出现：{only_doc:?}\n  只在 pub mod 出现：{only_code:?}\n\
         请同步 lib.rs 顶部分层表与 docs/ARCHITECTURE.md §1 的三圈分工。"
    );
}

#[test]
fn architecture_layering_covers_all_modules() {
    let path = repo_file(&["docs", "ARCHITECTURE.md"]);
    let arch = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读 {}: {e}", path.display()));
    let lib_path = repo_file(&["crates", "sl-core", "src", "lib.rs"]);
    let lib_src = fs::read_to_string(&lib_path)
        .unwrap_or_else(|e| panic!("读 {}: {e}", lib_path.display()));

    let declared = declared_modules(&lib_src);
    let layering = architecture_modules(&arch);


    let missing: Vec<_> = declared
        .iter()
        .filter(|m| !layering.contains(**m))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/ARCHITECTURE.md §1 的三圈分工漏掉了这些模块：{missing:?}\n\
         请把它们补进对应圈层的描述里。"
    );
}