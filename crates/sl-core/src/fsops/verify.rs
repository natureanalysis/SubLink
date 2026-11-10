//! 数据核对。
//!
//! 跨卷搬运的本质是"复制 + 删除"。删除之前必须证明复制是完整的 ——
//! 这里是唯一的证据来源，也是"绝不在核对通过前删源"这条铁律的执行者。

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::error::{io_ctx, SlError, SlResult};
use crate::i18n::tf;
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// 文件数 + 总字节数。快，覆盖绝大多数场景。
    CountAndSize,
    /// 逐文件 SHA-256。慢，用于不能重造的关键数据。
    Sha256,
}

impl VerifyMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "count" | "count_and_size" | "size" => Some(VerifyMode::CountAndSize),
            "sha" | "sha256" | "hash" => Some(VerifyMode::Sha256),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            VerifyMode::CountAndSize => crate::i18n::t("verify.mode.count"),
            VerifyMode::Sha256 => crate::i18n::t("verify.mode.sha256"),
        }
    }
}

/// 目录树的形状统计。链接不跟随、单独计入 `links`，不参与 `files/bytes`。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeStats {
    pub files: u64,
    pub bytes: u64,
    pub dirs: u64,
    /// 树内遇到的重解析点数量（不跟随）。
    pub links: u64,
}

impl TreeStats {
    pub fn summary(&self) -> String {
        let links = if self.links > 0 {
            crate::i18n::tf("tree.links", &[("n", &self.links.to_string())])
        } else {
            String::new()
        };
        crate::i18n::tf("tree.summary", &[
            ("files", &self.files.to_string()),
            ("size", &util::human_bytes(self.bytes)),
            ("dirs", &self.dirs.to_string()),
            ("links", &links),
        ])
    }
}

/// 测量目录树（只读）。
pub fn measure(dir: &Path) -> SlResult<TreeStats> {
    let mut st = TreeStats::default();
    if !dir.exists() {
        return Ok(st);
    }
    for entry in WalkDir::new(util::to_verbatim(dir))
        .follow_links(false)
        .into_iter()
    {
        let entry = match entry {
            Ok(e) => e,
            // 单个条目读不到（权限/被删）不应该让整棵树的测量失败
            Err(_) => continue,
        };
        // 根目录本身不计入 dirs —— "N 个子目录"才符合直觉，
        // 也保证源侧与目标侧的统计口径一致（两侧根都跳过）。
        if entry.depth() == 0 {
            continue;
        }
        let ft = entry.file_type();
        if ft.is_symlink() {
            st.links += 1;
        } else if ft.is_dir() {
            st.dirs += 1;
        } else if ft.is_file() {
            st.files += 1;
            st.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(st)
}

/// 目录树内容指纹：对「相对路径 + 文件长度 + 文件内容」按路径排序后计算 SHA-256。
/// 路径统一小写并用正斜杠，避免大小写与分隔符差异造成假阳性。
pub fn hash_tree(dir: &Path) -> SlResult<String> {
    let mut files: BTreeMap<String, u64> = BTreeMap::new();
    for entry in WalkDir::new(util::to_verbatim(dir))
        .follow_links(false)
        .into_iter()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(util::to_verbatim(dir))
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('/', "\\")
            .to_lowercase();
        files.insert(rel, entry.metadata().map(|m| m.len()).unwrap_or(0));
    }

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    for (rel, len) in &files {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update(len.to_le_bytes());
        hasher.update([0u8]);
        let abs = Path::new(dir).join(rel.replace('\\', "/"));
        let mut f = fs::File::open(util::to_verbatim(&abs))
            .map_err(|e| io_ctx(e, &abs, crate::i18n::t("io.action.read_hash_file")))?;
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|e| io_ctx(e, &abs, crate::i18n::t("io.action.read_hash_file")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 核对两侧统计是否一致。不一致直接返回错误（调用方据此**放弃删除源**）。
pub fn compare_stats(expected: &TreeStats, actual: &TreeStats, ctx: &str) -> SlResult<()> {
    if expected.files != actual.files {
        return Err(SlError::VerifyFailed(tf("verify.file_count_mismatch", &[
            ("ctx", ctx),
            ("expected", &expected.files.to_string()),
            ("actual", &actual.files.to_string()),
        ])));
    }
    if expected.bytes != actual.bytes {
        return Err(SlError::VerifyFailed(tf("verify.byte_count_mismatch", &[
            ("ctx", ctx),
            ("expected", &util::human_bytes(expected.bytes)),
            ("actual", &util::human_bytes(actual.bytes)),
        ])));
    }
    if expected.links != actual.links {
        return Err(SlError::VerifyFailed(tf("verify.link_count_mismatch", &[
            ("ctx", ctx),
            ("expected", &expected.links.to_string()),
            ("actual", &actual.links.to_string()),
        ])));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("td_verify_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn measure_counts_correctly() {
        let base = fixture("measure");
        fs::create_dir_all(base.join("a").join("b")).unwrap();
        fs::write(base.join("a").join("one.txt"), b"12345").unwrap();
        fs::write(base.join("a").join("b").join("two.txt"), b"123").unwrap();

        let st = measure(&base).unwrap();
        assert_eq!(st.files, 2);
        assert_eq!(st.bytes, 8);
        assert_eq!(st.dirs, 2); // a 与 a/b
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn hash_is_stable_and_sensitive() {
        let base = fixture("hash");
        fs::write(base.join("x.txt"), b"abc").unwrap();
        let h1 = hash_tree(&base).unwrap();
        let h2 = hash_tree(&base).unwrap();
        assert_eq!(h1, h2);

        fs::write(base.join("x.txt"), b"abd").unwrap();
        assert_ne!(h1, hash_tree(&base).unwrap());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn compare_reports_mismatch() {
        let a = TreeStats { files: 1, bytes: 10, dirs: 0, links: 0 };
        let b = TreeStats { files: 1, bytes: 11, dirs: 0, links: 0 };
        assert!(compare_stats(&a, &b, "测试").is_err());
        assert!(compare_stats(&a, &a, "测试").is_ok());
    }
}
