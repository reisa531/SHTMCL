# SHTMCL

使用 Rust、Tauri 和 React 开发的 Minecraft 启动器，当前版本为 `0.1.0` 预览版。

当前实现包括：

- 游戏版本检索、原版与 Fabric 安装，以及固定版本的 NeoForge 安装路径。
- 独立实例、复制、资源校验缓存、下载取消和安装恢复。
- Modrinth 模组搜索、详情、按实例筛选版本和依赖展示。
- 本地 Mod 元数据查看、启用/禁用与恢复。
- Windows 单 EXE 便携构建，Linux AppImage/deb，macOS Universal app/DMG 工作流。
- Arch Linux 的 `shtmcl-bin` 构建材料与安装包生成。

Mod 自动安装与必需依赖自动安装仍在开发中。微软登录、外部发布和房间客户端接入暂缓。
Windows/macOS 的首次云端构建及各平台原生游戏运行验收会随开发继续推进。

## 构建环境

CI 使用 Rust 1.90.0、Node.js 24、pnpm 12.15.0。图形依赖安装方法见
[Tauri 系统依赖说明](https://v2.tauri.app/start/prerequisites/)。

```bash
pnpm install --frozen-lockfile
pnpm typecheck
cargo test --locked --workspace --lib
pnpm tauri:dev
```

## 打包与发行

GitHub Actions 的 **Package desktop** 可手动运行，在三端执行检查并上传制品。
版本标签会在所有必需构建成功后创建草稿 Release。

- Windows：单个 `SHTMCL.exe`，使用系统 WebView2；数据位于程序旁的 `SHTMCL-data/`。
- Linux：AppImage 和 deb，使用 GTK/WebKitGTK。
- macOS：Intel 与 Apple Silicon 通用版，使用系统 WKWebView；当前配置 ad-hoc 签名。
- AUR：稳定版本生成 `shtmcl-bin` 的 PKGBUILD、`.SRCINFO`、MIT LICENSE 和 Arch 安装包。

构建命令、版本同步和签名状态见 [三端打包说明](docs/development/PACKAGING-WORKFLOWS.md)。
AUR 提交流程见 [AUR 发布说明](docs/development/AUR-PUBLISHING.md)。

## 工程结构

| 路径 | 内容 |
| --- | --- |
| `crates/domain` | 公共类型和命令/事件契约 |
| `crates/platform` | 文件系统、进程和平台接口 |
| `crates/core` | 实例、下载、启动、内容与其他核心模块 |
| `apps/desktop` | 桌面桥、React 界面和 Tauri 配置 |
| `contracts` | 契约说明和样例 |
| `scripts` | 检查、图标与打包工具 |
| `packaging/aur` | AUR 构建模板 |
| `tests` | 模块与打包测试 |

## 许可证

[MIT](LICENSE)。
