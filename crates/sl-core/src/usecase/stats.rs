//! 效益统计：把「重定向到 RAM 盘」带来的收益量化出来。
//!
//! 只报**可解释**的三个数字，不做"预估节省 SSD 寿命 X 年"这类外推（那需要 TLC 擦写
//! 次数、写入放大等一堆假设，估出来的数只会误导）：
//!
//! * **当前重定向规模** —— 已生效规则的缓存总量。这些字节现在落在 RAM 盘上，
//!   是"本来会写到 SSD"的部分里最实的一块；
//! * **累计搬运** —— 历次 `apply` 成功搬运的总量（`operations` 表 `kind='apply'`）；
//! * **累计清理释放** —— 历次 `clean` 删除的总量（`kind='clean'`）。
//!
//! 数据来源是**结构化列**（`file_count` / `bytes`）而不是 `events.message` —— 后者是
//! 给人读的文本，聚合不了；因此 `clean_run` 与 `apply` 一样落一条操作流水。

use crate::store::Store;
use crate::util;

use super::view::SavingsView;

/// 汇总效益统计（只读，无副作用）。
pub fn savings(store: &Store) -> crate::SlResult<SavingsView> {
    let mut redirected_rules = 0usize;
    let mut redirected_files = 0u64;
    let mut redirected_bytes = 0u64;
    for r in store.list_rules()? {
        // 只算"此刻确实在 RAM 盘上"的：期望开启 且 上次检测到链接正常。
        // 期望关闭 / 真实目录 / 断链都不算 —— 宁少算，不虚报。
        let linked = matches!(
            r.last_state.as_deref(),
            Some("link_ok") | Some("link_ok_empty")
        );
        if !r.want_enabled || !linked {
            continue;
        }
        redirected_rules += 1;
        redirected_files += r.last_files.unwrap_or(0).max(0) as u64;
        redirected_bytes += r.last_bytes.unwrap_or(0).max(0) as u64;
    }

    let (moved_files, moved_bytes) = store.sum_ops_done("apply")?;
    let (cleaned_files, cleaned_bytes) = store.sum_ops_done("clean")?;

    Ok(SavingsView {
        redirected_rules,
        redirected_files,
        redirected_bytes,
        redirected_text: util::human_bytes(redirected_bytes),
        moved_files,
        moved_bytes,
        moved_text: util::human_bytes(moved_bytes),
        cleaned_files,
        cleaned_bytes,
        cleaned_text: util::human_bytes(cleaned_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewRule, Store};

    fn new_rule(name: &str, src: &str) -> NewRule {
        NewRule {
            name: name.into(),
            category: "dev".into(),
            source_template: src.into(),
            target_root_id: 1,
            target_subpath: "t".into(),
            link_type: "junction".into(),
            redirect_mode: "link".into(),
            env_vars: Vec::new(),
            env_scope: "user".into(),
            want_enabled: true,
            rebuild_on_boot: true,
            notes: None,
            excludes: Vec::new(),
            auto_clean_policy: String::new(),
            auto_clean_limit_bytes: 0,
        }
    }

    #[test]
    fn counts_only_linked_and_enabled_rules() {
        let s = Store::open_in_memory().unwrap();
        s.insert_root("RAM", "R:", true, 90000).unwrap();
        let a = s.insert_rule(&new_rule("A", r"C:\Temp\A")).unwrap();
        let b = s.insert_rule(&new_rule("B", r"C:\Temp\B")).unwrap();
        let _c = s.insert_rule(&new_rule("C", r"C:\Temp\C")).unwrap();

        // A：链接正常且开启 → 计入
        s.set_rule_applied(a, 10, 1000).unwrap();
        // B：链接正常但期望关闭 → 不计
        s.set_rule_applied(b, 20, 2000).unwrap();
        s.set_rule_enabled(b, false).unwrap();
        // C：期望开启但状态还是普通目录 → 不计
        s.set_rule_state(3, "plain_dir", None).unwrap();

        let v = savings(&s).unwrap();
        assert_eq!(v.redirected_rules, 1);
        assert_eq!(v.redirected_files, 10);
        assert_eq!(v.redirected_bytes, 1000);
    }

    #[test]
    fn empty_store_reports_zeros() {
        let s = Store::open_in_memory().unwrap();
        s.insert_root("RAM", "R:", true, 90000).unwrap();
        let v = savings(&s).unwrap();
        assert_eq!(v.redirected_rules, 0);
        assert_eq!(v.moved_bytes, 0);
        assert_eq!(v.cleaned_bytes, 0);
    }

    #[test]
    fn sums_only_done_operations_by_kind() {
        let s = Store::open_in_memory().unwrap();
        s.insert_root("RAM", "R:", true, 90000).unwrap();
        let id = s.insert_rule(&new_rule("A", r"C:\Temp\A")).unwrap();

        // apply 两条：一条 done、一条 failed（失败的不该计入）
        let ok = s.op_begin(Some(id), "apply", None, None).unwrap();
        s.op_finish(ok, "done", Some(5), Some(500), None).unwrap();
        let bad = s.op_begin(Some(id), "apply", None, None).unwrap();
        s.op_finish(bad, "failed", Some(9), Some(900), Some("boom"))
            .unwrap();

        // clean 两条 done → 累计
        for _ in 0..2 {
            let op = s.op_begin(Some(id), "clean", None, None).unwrap();
            s.op_finish(op, "done", Some(3), Some(300), None).unwrap();
        }

        let v = savings(&s).unwrap();
        assert_eq!(v.moved_files, 5, "失败的操作不计入");
        assert_eq!(v.moved_bytes, 500);
        assert_eq!(v.cleaned_files, 6);
        assert_eq!(v.cleaned_bytes, 600);
    }
}
