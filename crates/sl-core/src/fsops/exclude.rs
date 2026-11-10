//! 清理排除模式（fnmatch 风格，gitignore 的一个可解释子集）。
//!
//! ## 语义
//!
//! 模式针对**相对清理根目录**的路径（`\` 与 `/` 等价）：
//!
//! * 纯名字模式（不含分隔符，如 `sessions`、`*.log`）——匹配**任意层级**：
//!   路径中任何一段命中即排除（目录命中则整棵子树不进入）。
//! * 含分隔符模式（如 `logs/old`、`*/cache`）——**从根锚定**：
//!   整条相对路径与模式做 fnmatch。目录命中同样剪掉整棵子树
//!   （`logs/old` 能保护 `logs/old/` 下的一切）。
//! * `*` 匹配单层内任意字符（不跨分隔符），`**` 单独成段时跨层，
//!   `?` 匹配单个字符。
//!
//! ## 只影响清理
//!
//! 排除列表**不改变 apply/restore 的搬运语义**：搬运永远全量
//! （建链要求源路径腾空）。它的职责是让 `clean` 不误删缓存目录里
//! 混着的"不能丢"的东西。想让重要数据不进 RAM 盘，应把规则的源路径
//! 收窄到纯缓存子目录，而不是依赖排除。
//!
//! ## 按类型排除（[`Excludes::with_exts`]）
//!
//! 除了模式，还可以给一列**扩展名**（`py`、`.log`……）。差别在于作用对象：
//! `names` 里的 `*.py` 会把名为 `x.py` 的**目录**整棵剪掉，而扩展名列表
//! 只跳过扩展名命中的**文件** —— 目录照常进入、照常剪空。清理弹窗里的
//! "保留这些类型的文件"用的就是它。

use crate::error::{SlError, SlResult};
use crate::i18n::{t, tf};

/// 单条模式的最大长度与单规则的模式数上限（防表单塞垃圾）。
const MAX_PATTERN_LEN: usize = 120;
const MAX_PATTERNS: usize = 16;
/// 单条扩展名的最大长度与单规则的扩展名数上限。
const MAX_EXT_LEN: usize = 24;
const MAX_EXTS: usize = 16;

#[derive(Debug, Clone, Default)]
pub struct Excludes {
    /// 含分隔符的模式（按 `/` 预拆段）。
    anchored: Vec<Vec<String>>,
    /// 纯名字模式（任意层级生效）。
    names: Vec<String>,
    /// 文件扩展名（小写、不含点）：**只对文件生效**，目录永不因扩展名被排除。
    /// 与 `names` 里的 `*.py` 的区别见模块文档「按类型排除」。
    exts: Vec<String>,
}

impl Excludes {
    pub fn new(patterns: &[String]) -> SlResult<Self> {
        if patterns.len() > MAX_PATTERNS {
            return Err(SlError::InvalidPath(tf(
                "exclude.too_many",
                &[("max", &MAX_PATTERNS.to_string())],
            )));
        }
        let mut ex = Excludes::default();
        for p in patterns {
            let norm = p.trim().replace('\\', "/");
            if norm.is_empty() {
                return Err(SlError::InvalidPath(t("exclude.empty").into()));
            }
            if norm.len() > MAX_PATTERN_LEN {
                return Err(SlError::InvalidPath(tf(
                    "exclude.too_long",
                    &[("max", &MAX_PATTERN_LEN.to_string()), ("pat", p)],
                )));
            }
            let segs: Vec<&str> = norm.split('/').collect();
            if segs.contains(&"..") {
                return Err(SlError::InvalidPath(tf("exclude.no_parent", &[("pat", p)])));
            }
            if segs.len() == 1 {
                ex.names.push(norm);
            } else {
                ex.anchored
                    .push(segs.into_iter().map(|s| s.to_string()).collect());
            }
        }
        Ok(ex)
    }

    pub fn is_empty(&self) -> bool {
        self.anchored.is_empty() && self.names.is_empty() && self.exts.is_empty()
    }

    /// 设置「按类型排除」的扩展名。每项只写扩展名，`py` / `.py` / `*.py` 都可，
    /// 统一转小写并剥掉前导的 `*` 与 `.`；空项忽略（逗号连打不算错），
    /// 其余必须是不含点号、不含分隔符、不含空白的单个扩展名。
    pub fn set_exts(&mut self, exts: &[String]) -> SlResult<()> {
        let mut norm: Vec<String> = Vec::new();
        for raw in exts {
            let tok = raw.trim();
            if tok.is_empty() {
                continue;
            }
            let t = tok.trim_start_matches(|c: char| c == '*' || c == '.');
            if t.is_empty()
                || t.contains('.')
                || t.contains('/')
                || t.contains('\\')
                || t.chars().any(|c| c.is_whitespace())
            {
                return Err(SlError::InvalidPath(tf("exclude.bad_ext", &[("ext", raw)])));
            }
            if t.len() > MAX_EXT_LEN {
                return Err(SlError::InvalidPath(tf(
                    "exclude.too_long",
                    &[("max", &MAX_EXT_LEN.to_string()), ("pat", raw)],
                )));
            }
            let lower = t.to_lowercase();
            if !norm.contains(&lower) {
                norm.push(lower);
            }
        }
        if norm.len() > MAX_EXTS {
            return Err(SlError::InvalidPath(tf(
                "exclude.too_many_exts",
                &[("max", &MAX_EXTS.to_string())],
            )));
        }
        self.exts = norm;
        Ok(())
    }

    /// [`Excludes::set_exts`] 的链式版本（构造点多为一次成型）。
    pub fn with_exts(mut self, exts: &[String]) -> SlResult<Self> {
        self.set_exts(exts)?;
        Ok(self)
    }

    /// **文件**条目（相对路径）是否应被排除：模式命中或扩展名命中。
    /// 目录一律只用 [`Excludes::matches`] —— 扩展名不作用于目录。
    pub fn matches_file(&self, rel: &str) -> bool {
        self.matches(rel) || self.matches_ext(rel)
    }

    /// 扩展名是否命中：只看**最后一段**的扩展名（`a.tar.gz` 的扩展名是 `gz`），
    /// 大小写不敏感；`.` 开头的隐藏文件（`.gitignore`）不算有扩展名。
    pub fn matches_ext(&self, rel: &str) -> bool {
        if self.exts.is_empty() {
            return false;
        }
        let name = rel
            .rsplit(['/', '\\'])
            .find(|s| !s.is_empty())
            .unwrap_or("");
        match name.rfind('.') {
            Some(0) | None => false,
            Some(pos) => {
                let ext = name[pos + 1..].to_lowercase();
                !ext.is_empty() && self.exts.iter().any(|e| *e == ext)
            }
        }
    }

    /// 相对路径（以 `/` 分隔）是否命中任一排除模式。
    pub fn matches(&self, rel: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let segs: Vec<&str> = rel.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
        // 纯名字模式：任意一段命中即排除（含最后一段 = 文件名）。
        for pat in &self.names {
            if segs.iter().any(|s| seg_match(pat, s)) {
                return true;
            }
        }
        // 锚定模式：整条相对路径匹配。
        for pat in &self.anchored {
            if segs_match(pat, &segs) {
                return true;
            }
        }
        false
    }
}

/// 段序列匹配：`**` 单独成段时跨层，其余段逐段 [`seg_match`]。
/// 模式耗尽而路径还有剩余段 → 视为**前缀命中**（目录模式保护整棵子树）。
fn segs_match(pat: &[String], segs: &[&str]) -> bool {
    match (pat.first(), segs.first()) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(p), Some(s)) => {
            if p == "**" {
                // `**` 吃掉 0..n 段。
                (0..=segs.len()).any(|skip| segs_match(&pat[1..], &segs[skip..]))
            } else {
                seg_match(p, s) && segs_match(&pat[1..], &segs[1..])
            }
        }
    }
}

/// 单段内 glob：`*` 任意长度、`?` 单字符，不含跨层语义。
fn seg_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    // 经典双指针 + 回溯处理 `*`。
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(pats: &[&str]) -> Excludes {
        let v: Vec<String> = pats.iter().map(|s| s.to_string()).collect();
        Excludes::new(&v).unwrap()
    }

    #[test]
    fn plain_name_matches_any_level() {
        let e = ex(&["sessions"]);
        assert!(e.matches("sessions"));
        assert!(e.matches("profiles/x/sessions"));
        assert!(e.matches("sessions/db.sqlite"));
        assert!(!e.matches("session"));
        assert!(!e.matches("a/sessions2/b"));
    }

    #[test]
    fn star_does_not_cross_separator() {
        let e = ex(&["*.log"]);
        assert!(e.matches("a.log"));
        assert!(e.matches("sub/b.log"));
        assert!(!e.matches("a/logs")); // `*` 不跨层
    }

    #[test]
    fn anchored_pattern_matches_from_root_only() {
        let e = ex(&["logs/old"]);
        assert!(e.matches("logs/old"));
        assert!(e.matches("logs/old/x.txt"));
        assert!(!e.matches("deep/logs/old"), "锚定模式不从任意层级起算");
        assert!(!e.matches("logs"));
    }

    #[test]
    fn double_star_crosses_levels() {
        let e = ex(&["logs/**/old"]);
        assert!(e.matches("logs/old"));
        assert!(e.matches("logs/a/b/old"));
        assert!(e.matches("logs/a/b/old/x"));
        assert!(!e.matches("a/logs/old"), "首段仍是锚定的");
    }

    #[test]
    fn question_mark_single_char() {
        let e = ex(&["v?"]);
        assert!(e.matches("v1"));
        assert!(e.matches("x/v9"));
        assert!(!e.matches("v10"));
    }

    #[test]
    fn backslash_is_normalized() {
        let e = ex(&["logs\\old"]);
        assert!(
            e.matches("logs/old"),
            "Windows 风格模式与 Unix 风格路径等价"
        );
        assert!(e.matches("logs\\old"), "反斜杠路径同样命中");
    }

    #[test]
    fn empty_excludes_match_nothing() {
        let e = Excludes::default();
        assert!(!e.matches("anything/at/all"));
        assert!(e.is_empty());
    }

    #[test]
    fn validation_rejects_garbage() {
        let mk = |pats: &[&str]| {
            let v: Vec<String> = pats.iter().map(|s| s.to_string()).collect();
            Excludes::new(&v)
        };
        assert!(mk(&[""]).is_err(), "空模式");
        assert!(mk(&["a/.."]).is_err(), ".. 段");
        assert!(mk(&["ok", ""]).is_err(), "混合里的空模式");
        let many: Vec<String> = (0..17).map(|i| format!("p{i}")).collect();
        assert!(Excludes::new(&many).is_err(), "超过 16 条");
        let long = "x".repeat(MAX_PATTERN_LEN + 1);
        assert!(mk(&[&long]).is_err(), "超长模式");
        assert!(mk(&["ok"]).is_ok());
    }

    #[test]
    fn seg_match_basics() {
        assert!(seg_match("*", ""));
        assert!(seg_match("a*c", "abc"));
        assert!(seg_match("a*c", "ac"));
        assert!(!seg_match("a*c", "abd"));
        assert!(seg_match("a?c", "a+c"));
        assert!(!seg_match("a?c", "ac"));
        assert!(seg_match("平台无关", "平台无关"), "非 ASCII 同样匹配");
    }

    #[test]
    fn ext_list_matches_last_segment_only() {
        let e = ex(&[]).with_exts(&["py".into(), ".LOG".into()]).unwrap();
        assert!(e.matches_ext("a.py"));
        assert!(e.matches_ext("deep/dir/b.py"), "任意层级");
        assert!(e.matches_ext("x.LOG"), "大小写不敏感");
        assert!(e.matches_ext("deep\\x.Log"), "反斜杠路径同样认最后一段");
        assert!(!e.matches_ext(".gitignore"), "点开头的隐藏文件没有扩展名");
        assert!(!e.matches_ext("noext"));
        assert!(!e.matches_ext("a.tar.gz"), "只看最后一段：gz 不在列表里");
        assert!(!e.matches_ext(""));
        assert!(
            !Excludes::default().matches_ext("a.py"),
            "没配类型时一律不命中"
        );

        let gz = ex(&[]).with_exts(&["gz".into()]).unwrap();
        assert!(gz.matches_ext("a.tar.gz"), "只看最后一段：gz 命中");
        assert!(!gz.matches_ext("a.gz2"));
    }

    #[test]
    fn ext_and_pattern_are_independent() {
        let e = ex(&["sessions"]).with_exts(&["log".into()]).unwrap();
        assert!(
            e.matches_file("sessions/db.sqlite"),
            "模式命中 → 文件被排除"
        );
        assert!(e.matches_file("deep/app.log"), "扩展名命中 → 文件被排除");
        assert!(e.matches_file("sessions/app.log"), "两者都命中");
        assert!(!e.matches_file("deep/app.bin"));
        // 目录判定只走模式：`matches` 不认扩展名，免得名为 app.log 的目录被剪。
        assert!(e.matches("sessions/db.sqlite"));
        assert!(!e.matches("deep/app.log"), "扩展名不参与目录判定");
    }

    #[test]
    fn ext_validation_rejects_garbage() {
        let mk = |v: &[&str]| {
            let v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
            Excludes::default().with_exts(&v)
        };
        assert!(mk(&["py", ".log", "*.tmp"]).is_ok(), "三种写法都接受");
        assert!(mk(&["", "  "]).is_ok(), "空项忽略");
        assert!(mk(&["a/b"]).is_err(), "含分隔符");
        assert!(mk(&["tar.gz"]).is_err(), "含第二个点");
        assert!(mk(&["*"]).is_err(), "剥完是空的");
        assert!(mk(&["p y"]).is_err(), "含空白");
        let many: Vec<String> = (0..17).map(|i| format!("e{i}")).collect();
        assert!(Excludes::default().with_exts(&many).is_err(), "超过 16 种");
        let long = "x".repeat(MAX_EXT_LEN + 1);
        assert!(mk(&[&long]).is_err(), "超长扩展名");

        let e = mk(&["PY", ".py", "py"]).unwrap();
        assert_eq!(e.exts, vec!["py".to_string()], "统一小写并去重");
        assert!(!e.is_empty(), "配了类型就不算空");
    }
}
