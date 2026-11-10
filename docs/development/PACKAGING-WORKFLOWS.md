# 三端打包工作流

适用当前 standard 版。三端从同一提交、同一版本构建；CLI-supported 在对应 feature 实现后再加入矩阵。

## 构建入口与制品

| 平台 | GitHub runner | 架构 | 输出 |
| --- | --- | --- | --- |
| Windows | `windows-2022` | x86_64 MSVC | 单个 `SHTMCL.exe` |
| Linux | `ubuntu-22.04` | x86_64 | `.AppImage`、`.deb` |
| macOS | `macos-15` | Universal：Intel + Apple Silicon | `.dmg`、保留 app 目录和权限的 `.app.zip` |

稳定版本还会在 Arch Linux 容器中构建 `shtmcl-bin`，生成 `.pkg.tar.zst` 与 AUR recipe 压缩包。
recipe 包含 PKGBUILD、`.SRCINFO`、MIT LICENSE；使用和发布步骤见 [AUR 打包与发布](./AUR-PUBLISHING.md)。

- `.github/workflows/package.yml`：手动、main/master 相关代码 push、PR 和可复用调用入口。先检查版本、前端与契约，再在三端执行本机 Rust 测试和打包，上传 Actions artifacts，保留 14 天。
- `.github/workflows/release.yml`：推送 `v*` 标签后复用完整打包工作流；三端成功、下载后的 SHA-256 校验通过，才创建或更新该标签的**草稿 Release**。正式发布由维护者在 GitHub 完成。已发布 Release 不会被覆盖。

Windows 沿用 `scripts/build-portable-win.ps1`，继续使用系统 WebView2，不生成安装向导，不携带 Java、游戏、用户实例或 `SHTMCL-data`。检查的是未压缩 EXE：25 MiB 为目标，超过 40 MiB 构建失败。

Linux 使用 Tauri 的平台配置，deb 声明由 bundler 生成的 GTK/WebKit 依赖。AppImage 携带更多运行库，其大小单独记录，不套用 Windows EXE 的体积门槛。

macOS 使用系统 WKWebView，最低版本设置为 macOS 11；流水线校验最终可执行文件包含 arm64 和 x86_64，并执行 `codesign --verify`。当前使用 ad-hoc 签名，不需要 Apple 账号 Secrets，也没有 Developer ID 公证。公开正式发行前可按 [Tauri macOS 签名说明](https://v2.tauri.app/distribute/sign/macos/) 接入 Developer ID 与 notarization；ad-hoc 签名不等于已获 Gatekeeper 信任。

Windows 的数据继续位于 EXE 旁；Linux/macOS 沿用现有 Tauri 用户数据目录。本次仅调整打包配置，没有改动数据位置和游戏运行逻辑。

## 如何运行

将源码和工作流提交到 GitHub 仓库默认分支后，在 **Actions → Package desktop → Run workflow** 选择分支运行。
完成后，在该次运行底部的 Artifacts 下载：

```text
shtmcl-0.1.0-windows-x86_64
shtmcl-0.1.0-linux-x86_64
shtmcl-0.1.0-macos-universal
shtmcl-0.1.0-aur-x86_64
```

每组包含实际制品、`build-<platform>-<arch>.json` 和 `SHA256SUMS-<platform>-<arch>.txt`。
构建记录含源码 commit、版本、目标、variant、大小和 SHA-256；Step summary 也会显示制品体积。
GitHub Actions 的外层 artifact ZIP 不保留 Unix 可执行位；AppImage 下载解压后按需 `chmod +x`，macOS 使用内部 app ZIP 或 DMG。

发行新版本时，同步以下四处版本：

1. 根 `Cargo.toml` 的 `[workspace.package].version`。
2. 根 `package.json`。
3. `apps/desktop/package.json`。
4. `apps/desktop/src-tauri/tauri.conf.json`。

修改 Cargo 版本后执行 `cargo update --workspace`，把工作区包版本同步到 `Cargo.lock`，与版本和源码一起提交。
再推送完全一致的标签，例如当前版本为 `0.1.0` 时使用：

```bash
git tag v0.1.0
git push origin v0.1.0
```

`v0.1.1` 标签配 `0.1.0` 源码会直接失败；包含预发行后缀的版本会创建 prerelease 草稿。
草稿构建重跑时更新其附件；推送标签后不会自动变成公开 Release。构建阶段仅需要 `contents: read`，草稿阶段单独使用 GitHub 自动提供的 `contents: write` token。

## 本机构建

Node 24、pnpm 12.15.0、Rust 1.90.0 是 CI 使用的版本。锁文件包含 pnpm 自身版本的锁定信息；安装使用 `pnpm install --frozen-lockfile`，Cargo 使用 `--locked`。

### Windows x64

安装 Rust MSVC、Visual Studio C++ 工具和 Node 后，在仓库根目录执行：

```powershell
pnpm install --frozen-lockfile
cargo fetch --locked --target x86_64-pc-windows-msvc
./scripts/build-portable-win.ps1
node scripts/package-artifacts.mjs windows
```

Windows 原脚本保留离线 Cargo 构建，因此先 fetch。制品收集只读取 `target/portable/SHTMCL.exe`。

### Linux

Ubuntu 22.04 系统依赖及版本安装步骤已列在 workflow 中。安装后执行：

```bash
pnpm install --frozen-lockfile
cd apps/desktop
APPIMAGE_EXTRACT_AND_RUN=1 pnpm exec tauri build --ci \
  --target x86_64-unknown-linux-gnu --bundles appimage deb -- --locked
cd ../..
node scripts/package-artifacts.mjs linux
```

### macOS

需要 Xcode command line tools 和两个 Rust target：

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
pnpm install --frozen-lockfile
cd apps/desktop
MACOSX_DEPLOYMENT_TARGET=11.0 pnpm exec tauri build --ci \
  --target universal-apple-darwin --bundles app dmg -- --locked
cd ../..
node scripts/package-artifacts.mjs macos
```

制品收集到根目录 `release-assets/`，该目录已加入忽略规则。一次收集需要空目录，避免混入旧版本；本地再次收集前移走上次输出。CI runner 每次使用独立工作区。

## 实现与维护

- `tauri.linux.conf.json` 和 `tauri.macos.conf.json` 由 Tauri 按平台合并，不改变 Windows 的 `bundle.active=false`。
- `scripts/gen-icons.mjs` 继续使用既有品牌图形，新增依赖内置 Node API 生成的 `icon.icns`，检查入口会验证 PNG/ICO/ICNS。
- `scripts/release-version.mjs` 负责源版本、标签和工作流 version 输出。
- `scripts/package-artifacts.mjs` 负责平台制品选择、大小记录、流式 SHA-256 与 macOS ditto 归档；不解析或复制用户数据。
- Actions 依赖固定在已核对的 commit SHA，旁边保留主版本注释。更新时同步核对 Action 的 Node/runner 要求。
- macOS runner 使用当前受支持的 `macos-15`。runner 生命周期依据 [GitHub runner-images](https://github.com/actions/runner-images#available-images)；构建和依赖配置参考 [Tauri GitHub 构建指南](https://v2.tauri.app/distribute/pipelines/github/) 与 [系统依赖](https://v2.tauri.app/start/prerequisites/)。

本轮本地验证记录见 `work/validation/00/packaging-workflows-20261110-01/`。Linux 原生构建及两种格式已实测；Windows/macOS 的本工作流首次云端执行结果仍需在仓库 Actions 中确认。
