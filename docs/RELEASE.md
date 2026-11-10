# 发布流程（Release Runbook）

公开发布的产物：**NSIS 安装包 + 便携 ZIP + SHA-256 校验和**。由 GitHub Actions 在推送
版本 tag 后自动构建，Release 以**草稿**形式创建，人工核验后再点发布。

## 版本号位置（两处必须一致）

| 文件 | 字段 |
|---|---|
| `Cargo.toml` | `[workspace.package] version` |
| `crates/sl-gui/tauri.conf.json` | `version` |

成员 crate（sl-core / sl-cli / sl-gui）用 `version.workspace = true` 继承，不用逐个改。

## 发布步骤

1. 更新上面两处版本号，并在 `CHANGELOG.md` 顶部按日期加条目。
2. commit 并 push 到 `main`，确认 CI（`.github/workflows/ci.yml`）全绿 ——
   CI 跑 `cargo test --workspace` + `cargo check --workspace --all-targets` +
   `node --check`（静态 UI 文件）。
3. 打 tag 并推送（tag 必须是 `v` + 版本号，且与两处版本号一致）：

   ```powershell
   git tag v0.1.0
   git push origin v0.1.0
   ```

4. Actions 的 Release workflow 自动执行：`cargo tauri build --bundles nsis` →
   生成安装包 → 打包便携 ZIP → 计算 SHA-256 → 创建草稿 Release 并上传全部产物。
5. 到仓库 Releases 页核验草稿：
   * 下载安装包，实装一遍 —— 装完打开界面、加一条规则、从"设置"看自启状态，
     最后卸载验证无残留；
   * 便携 ZIP 解压到别的目录跑一遍；
   * 用 `SHA256SUMS.txt` 核对下载完整性（`certutil -hashfile <文件> SHA256`）。
6. 补发布说明（或保留 workflow 自动生成的 notes），点 **Publish release**。

## 本地构建（可选）

CI 是主路径；本地构建需要一个**普通终端**——会拦截 `reg.exe` 的沙箱 shell 不行：
`vcvars64.bat` 要靠 `reg.exe` 定位 Windows SDK，被拦会报
`LNK1181: cannot open input file 'kernel32.lib'`（`scripts\cargo.cmd` 已尽量兜底，
但兜不住 reg.exe 被拦）。

```powershell
scripts\cargo.cmd tauri build --bundles nsis
```

产物：

| 文件 | 说明 |
|---|---|
| `target\release\bundle\nsis\SubLink_<版本>_x64-setup.exe` | NSIS 安装包 |
| `target\release\sublink.exe` | CLI（管理 / 提权 worker / 开机重建） |
| `target\release\sublink-gui.exe` | 界面（release 下无控制台窗口） |

便携 ZIP 就是把两个 exe 打包（CI 里还会塞一份 `README-portable.txt`）。

## 已知事项

* **安装包未做 Authenticode 签名**：用户首次运行会看到 SmartScreen
  "Windows 已保护你的电脑"（更多信息 → 仍要运行）。要消除需购买代码签名证书，
  之后可在 release workflow 里加签名步骤（`azure/trusted-signing-action` 或
  `Certificate` + `signtool`）。
* **自动更新未接入**（tauri updater）：升级靠手动下载新版，发布说明里写清即可。
* 预装版与便携版数据目录相同（`%PROGRAMDATA%\SubLink`），可互相覆盖升级；
  便携模式用 `SUBLINK_DATA_DIR` 隔离。
