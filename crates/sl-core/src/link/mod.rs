//! 链接引擎：重解析点识别、目录 Junction 建立与拆除。

pub mod create;
pub mod detect;

pub use create::{create_dir_link, preflight, read_reparse_tag, remove_dir_link};
pub use detect::{detect_link, LinkState, LinkType, TargetState};
