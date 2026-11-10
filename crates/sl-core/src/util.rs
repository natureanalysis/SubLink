//! 路径工具：长路径、归一化、环境变量展开、卷判定。
//!
//! 这里所有函数都**不做 canonicalize**。`std::fs::canonicalize` 会解析重解析点并
//! 展开 8.3 短名，对我们这种"要拿链接本身、而不是链接指向的目标"的场景语义是错的。

use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;

/// 绝对化并加上 `\\?\` verbatim 前缀，用于绕开 `MAX_PATH = 260`。
///
/// - `C:\a\b`      → `\\?\C:\a\b`
/// - `\\srv\share` → `\\?\UNC\srv\share`
/// - 已是 verbatim 的原样返回
pub fn to_verbatim(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let s = abs.to_string_lossy();
    if s.starts_with(r"\\?\") {
        return PathBuf::from(s.as_ref());
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    PathBuf::from(format!(r"\\?\{s}"))
}

/// 去掉 verbatim 前缀，得到可用于展示与入库的普通路径。
pub fn simplify(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p.to_path_buf()
}

/// 归一化键：统一反斜杠、去尾分隔符、转小写（Windows 路径大小写不敏感）。
/// 用于去重、比较、黑名单匹配 —— 不要用它给系统 API 传参。
pub fn normalize_key(p: &str) -> String {
    let mut s = p.trim().replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s.to_lowercase()
}

/// 大小写不敏感、忽略尾分隔符的路径等价判断。
pub fn path_eq(a: &Path, b: &Path) -> bool {
    normalize_key(&simplify(a).to_string_lossy()) == normalize_key(&simplify(b).to_string_lossy())
}

/// 以 NUL 结尾的宽字符串（Win32 W 系 API 的入参）。
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 从定长宽字符缓冲区读出一个以 NUL 结尾的字符串。
pub fn from_wide_buf(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 展开 `%VAR%`。返回（展开结果，未能解析的变量名列表）。
///
/// 注意：以 **SYSTEM** 身份运行时 `%APPDATA%` 会展开成
/// `C:\Windows\system32\config\systemprofile\AppData\...`，这是错的。
/// 所以开机重建必须在**用户上下文**里执行（详见 `boot::rebuild`）。
pub fn expand_env(tpl: &str) -> (String, Vec<String>) {
    let chars: Vec<char> = tpl.chars().collect();
    let mut out = String::with_capacity(tpl.len());
    let mut unknown = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '%' {
            if let Some(rel) = chars[i + 1..].iter().position(|c| *c == '%') {
                let end = i + 1 + rel;
                if end > i + 1 {
                    let name: String = chars[i + 1..end].iter().collect();
                    match std::env::var(&name) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => {
                            unknown.push(name.clone());
                            out.push('%');
                            out.push_str(&name);
                            out.push('%');
                        }
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    (out, unknown)
}

/// 展开模板并返回规范化后的绝对路径。
pub fn expand_path(tpl: &str) -> (PathBuf, Vec<String>) {
    let (s, unknown) = expand_env(tpl);
    (PathBuf::from(s), unknown)
}

/// 规范化"目标根"路径。
///
/// 关键点：裸盘符必须补上根分隔符。`PathBuf::from("R:").join("a\\b")` 的结果是
/// **`R:a\b`**（反斜杠被吃掉了），那是"R 盘当前目录下的 a\b"，不是"R 盘根下的 a\b"。
/// 这个坑在目标根写成 `R:` 时 100% 会踩到，所以统一在入口处规范化。
pub fn normalize_root_path(s: &str) -> String {
    let t = s.trim();
    let mut out = t.replace('/', "\\");
    if out.len() == 2 && out.ends_with(':') {
        out.push('\\');
        return out;
    }
    while out.len() > 3 && out.ends_with('\\') {
        out.pop();
    }
    out
}

/// 路径所在的卷根（挂载点），形如 `c:\`。失败返回 `None`。
///
/// 三个必须注意的点：
/// 1. 返回值是**可直接当路径用**的，且**保留尾部分隔符** —— `C:\` 与 `C:`
///    在 Windows 上是两回事（后者是"C 盘当前目录"）。所以这里不能直接用
///    `normalize_key`（它会因为长度 > 3 把尾部 `\` 去掉）。
/// 2. `GetVolumePathNameW` 对 verbatim 输入会返回 verbatim 形式的卷根
///    （`\\?\C:\`），必须先剥掉；对不存在的路径在部分环境下也会失败，
///    所以逐级向上找到第一个存在的祖先更稳妥。
/// 3. **有些 RAM 盘驱动压根不支持这个 API** —— 实测某款 RAM 盘上
///    `GetVolumePathNameW("V:\")` 直接返回 `ERROR_INVALID_FUNCTION(1)`。
///    这时退回"从路径语法上取盘符根"，否则 RAM 盘会被误判成"卷不存在"，
///    开机重建会去傻等一个永远不来的目录。
pub fn volume_root(p: &Path) -> Option<String> {
    if let Some(root) = volume_root_via_api(p) {
        return Some(root);
    }
    syntactic_root(&simplify(&to_verbatim(p))).map(|r| r.to_string_lossy().to_string())
}

fn volume_root_via_api(p: &Path) -> Option<String> {
    let mut probe = simplify(&to_verbatim(p));
    loop {
        if let Some(root) = volume_root_exact(&probe) {
            // 剥掉 verbatim 前缀，但保留尾部反斜杠
            return Some(simplify(Path::new(&root)).to_string_lossy().to_string());
        }
        match probe.parent() {
            // `parent()` 回到自身/空路径时退出，避免死循环
            Some(parent) if parent.as_os_str().len() < probe.as_os_str().len() => {
                probe = parent.to_path_buf();
            }
            _ => return None,
        }
    }
}

/// 从路径语法上取卷根：`V:\a\b` → `V:\`；`\\srv\share\x` → `\\srv\share\`。
/// 不查任何系统 API，纯粹按路径前缀判断（所以对"驱动不认 API"的卷也能用）。
pub fn syntactic_root(p: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    match p.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(PathBuf::from(format!("{}:\\", letter as char)))
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some(PathBuf::from(
                format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                ),
            )),
            _ => None,
        },
        _ => None,
    }
}

fn volume_root_exact(p: &Path) -> Option<String> {
    let wide = to_wide(&to_verbatim(p).to_string_lossy());
    let mut buf = [0u16; 260];
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return None;
    }
    let s = from_wide_buf(&buf);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 两个路径是否位于同一个卷。同卷可以直接 `rename`（原子、保留全部元数据），
/// 跨卷才需要"复制 → 校验 → 删除"三步。
pub fn same_volume(a: &Path, b: &Path) -> bool {
    match (volume_root(a), volume_root(b)) {
        // 比较时才做归一化（大小写不敏感、忽略尾部分隔符）
        (Some(x), Some(y)) => normalize_key(&x) == normalize_key(&y),
        _ => false,
    }
}

/// 人类可读的字节数。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 把秒级耗时格式化成 `1.2s` / `350ms`。
pub fn human_dur_ms(ms: u128) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 注意：下面凡是用普通字符串字面量表示 Windows 路径的地方都写成 `\\`，
    // 否则 `\t` / `\r` / `\n` 会被当成转义字符，测试会"通过语法检查但语义错误"。
    // 用 raw string（`r"..."`）也可以，但结尾是 `\` 时不直观，故统一用转义写法。

    #[test]
    fn verbatim_roundtrip() {
        let p = Path::new("C:\\Users\\a\\b");
        let v = to_verbatim(p);
        assert_eq!(v.to_string_lossy(), "\\\\?\\C:\\Users\\a\\b");
        assert_eq!(simplify(&v), p);
    }

    #[test]
    fn verbatim_unc() {
        let p = Path::new("\\\\srv\\share\\dir");
        let v = to_verbatim(p);
        assert_eq!(v.to_string_lossy(), "\\\\?\\UNC\\srv\\share\\dir");
        assert_eq!(simplify(&v), p);
    }

    #[test]
    fn verbatim_is_idempotent() {
        let v = to_verbatim(Path::new("C:\\a"));
        assert_eq!(to_verbatim(&v), v);
    }

    #[test]
    fn normalize_trims_and_lowers() {
        assert_eq!(normalize_key("C:\\Temp\\"), "c:\\temp");
        assert_eq!(normalize_key("c:/temp/"), "c:\\temp");
        assert_eq!(normalize_key("C:\\"), "c:\\");
        assert_eq!(normalize_key("R:/Redirect/x"), "r:\\redirect\\x");
    }

    #[test]
    fn path_eq_is_case_insensitive() {
        assert!(path_eq(Path::new("C:\\Temp\\"), Path::new("c:\\TEMP")));
        assert!(!path_eq(Path::new("C:\\Temp"), Path::new("C:\\Temp2")));
        // 中文路径不应 panic（切片要落在字符边界上）
        assert!(path_eq(
            Path::new("C:\\用户\\缓存"),
            Path::new("c:\\用户\\缓存")
        ));
    }

    #[test]
    fn expand_env_keeps_unknown() {
        let (s, unknown) = expand_env("%TEMP_DIRECTOR_NOT_EXIST_XYZ%\\sub");
        assert_eq!(s, "%TEMP_DIRECTOR_NOT_EXIST_XYZ%\\sub");
        assert_eq!(unknown, vec!["TEMP_DIRECTOR_NOT_EXIST_XYZ".to_string()]);
    }

    #[test]
    fn expand_env_replaces_known() {
        std::env::set_var("TD_TEST_VAR_ABC", "VALUE");
        let (s, unknown) = expand_env("x\\%TD_TEST_VAR_ABC%\\y");
        assert_eq!(s, "x\\VALUE\\y");
        assert!(unknown.is_empty());
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn human_dur_formats() {
        assert_eq!(human_dur_ms(350), "350ms");
        assert_eq!(human_dur_ms(1500), "1.5s");
    }

    #[test]
    fn normalize_root_path_adds_separator_for_bare_drive() {
        assert_eq!(normalize_root_path("R:"), "R:\\");
        assert_eq!(normalize_root_path("R:/"), "R:\\");
        assert_eq!(normalize_root_path("R:\\Redir\\"), "R:\\Redir");
        assert_eq!(normalize_root_path("R:/Redir"), "R:\\Redir");
        assert_eq!(normalize_root_path("C:\\"), "C:\\");
        // 裸盘符 join 之后必须还在根下
        assert_eq!(
            PathBuf::from(normalize_root_path("R:")).join("a\\b"),
            PathBuf::from("R:\\a\\b")
        );
    }

    #[test]
    fn syntactic_root_extracts_drive_and_unc() {
        use std::path::PathBuf;
        assert_eq!(
            syntactic_root(Path::new(r"V:\a\b")).unwrap(),
            PathBuf::from(r"V:\")
        );
        assert_eq!(
            syntactic_root(Path::new(r"C:\")).unwrap(),
            PathBuf::from(r"C:\")
        );
        assert_eq!(
            syntactic_root(Path::new(r"\\srv\share\x\y")).unwrap(),
            PathBuf::from(r"\\srv\share\")
        );
        assert!(syntactic_root(Path::new(r"relative\path")).is_none());
    }

    #[test]
    fn volume_root_falls_back_to_syntax_for_paths() {
        // 卷根必须是"能直接当路径用"的，且带尾部分隔符
        let root = volume_root(Path::new(r"V:\some\maybe\missing")).expect("应有卷根");
        assert!(root.ends_with('\\'), "卷根要保留尾部分隔符：{root}");
        assert_eq!(root.len(), 3, "盘符根应该是 V:\\ 这样的 3 个字符：{root}");
    }    #[test]
    fn trim_leading_and_trailing() {
        assert_eq!(from_wide_buf(&[65, 66, 0, 67]), "AB");
        assert_eq!(to_wide("A"), vec![65u16, 0]);
    }

}