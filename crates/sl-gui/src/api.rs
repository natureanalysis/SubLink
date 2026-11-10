//! 前端契约视图层（薄）。
//!
//! 业务逻辑已下沉到 [`sl_core::usecase`]，这里只做**再导出**：
//! 既保持命令层（`commands.rs`）与集成测试既有的 `api::Xxx` 符号路径不变，
//! 也让 GUI 侧不再直接编排 core 的领域模块（`engine` / `fsops` / `link` …）。
//!
//! 若将来需要 GUI 专属适配（例如把某个 usecase 结果再包一层），在本文件里加
//! 薄封装即可；**不要**把业务逻辑写回这里。

pub use sl_core::usecase::*;
