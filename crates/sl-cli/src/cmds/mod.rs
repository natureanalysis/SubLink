//! 子命令实现。按领域分组：
//! * [`roots`] / [`rules`] —— 目标根与规则的增删改查
//! * [`status`] —— 体检与观测：doctor / status / locks / volume / events / ops
//! * [`apply`] —— 应用 / 恢复 / 重链
//! * [`clean`] —— 文件夹清理（预演默认，`--yes` 才执行）
//! * [`acl`] —— 权限体检 / 修复
//! * [`boot`] —— 开机重建与登录任务
//! * [`config`] —— 配置导出 / 导入

pub(crate) mod acl;
pub(crate) mod apply;
pub(crate) mod boot;
pub(crate) mod clean;
pub(crate) mod config;
pub(crate) mod roots;
pub(crate) mod rules;
pub(crate) mod status;