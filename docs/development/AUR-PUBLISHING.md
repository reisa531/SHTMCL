# AUR 打包与发布

项目采用 MIT 许可证。AUR 包名使用 `shtmcl-bin`，复用 GitHub Release 中的 Linux x86_64 deb，将程序、桌面入口、图标和 MIT 许可证装入标准 Arch 路径。用户可以运行 `SHTMCL`，也可以使用小写别名 `shtmcl`。

AUR 保存 PKGBUILD、`.SRCINFO` 和必要的本地源文件。二进制文件放在 GitHub Release；CI 同时生成的 `.pkg.tar.zst` 可用于直接安装，但不提交到 AUR Git 仓库。此路线符合 [AUR 的预编译开源软件 `-bin` 命名要求](https://wiki.archlinux.org/title/AUR_submission_guidelines)。

## 本项目工作流

`package.yml` 的三端构建成功后，稳定版本 `x.y.z` 增加一个 Arch Linux 容器任务：

1. 下载同一次构建的 Linux 制品，核对 deb 与构建清单中的 SHA-256。
2. 从 `packaging/aur/PKGBUILD.in` 生成 PKGBUILD，并复制项目 LICENSE。
3. 使用 `makepkg --printsrcinfo` 生成 `.SRCINFO`。
4. 以非 root 用户运行 makepkg，构建 `shtmcl-bin`，再用 namcap 检查；出现 error 则失败。
5. 上传 `.pkg.tar.zst`、仅包含三份 AUR 文件的 recipe 压缩包、构建信息和校验和。

Actions artifact 名为 `shtmcl-<版本>-aur-x86_64`。版本标签工作流会把它与三端原有制品一起放入草稿 Release。
`-beta`、`-rc` 或带构建元数据的版本仍构建原有三端包，跳过稳定版 AUR 包，避免混用 SemVer 和 Arch 版本排序。

生成器读取 `GITHUB_REPOSITORY`，自动构造当前仓库的 Release 下载地址。发布包前，先将 GitHub Release 公开，确认其中的 `SHTMCL_<版本>_linux_x86_64.deb` 无需登录即可下载。
CI 在正式 Release 可见前，用相同文件名的本地缓存 deb 完成校验和验证，因此不依赖尚未公开的下载地址。

当前工作流只生成和验证 AUR 材料，不读取 SSH 私钥，不推送 AUR。许可证文本见根 `LICENSE`，AUR 归档中也携带同一文件。

## 通常怎样首次发布 AUR 包

### 1. 准备账号、工具和 SSH

先搜索官方仓库和 AUR，确认没有已维护的同名包。在 [AUR](https://aur.archlinux.org/) 注册账号，将用于维护包的 SSH 公钥添加到账户设置。

Arch 本机工具：

```bash
sudo pacman -S --needed base-devel git namcap pacman-contrib
```

需要单独的 AUR 密钥时，可以生成：

```bash
ssh-keygen -t ed25519 -f ~/.ssh/aur
```

把 `~/.ssh/aur.pub` 内容添加到 AUR。`~/.ssh/config` 配置示例：

```sshconfig
Host aur.archlinux.org
    User aur
    IdentityFile ~/.ssh/aur
    IdentitiesOnly yes
```

AUR 的 SSH 用户名固定是 `aur`，由公钥识别实际维护者。可以使用 `ssh aur@aur.archlinux.org help` 检查认证。

### 2. 克隆独立的 AUR 仓库

每个 pkgbase 有自己的 Git 仓库。包名可用时，首次推送会建立包：

```bash
git -c init.defaultBranch=master clone ssh://aur@aur.archlinux.org/shtmcl-bin.git
cd shtmcl-bin
```

AUR 使用 `master` 分支，和项目主仓库使用 main/master 无关。已被维护的同名包需要与现有维护者协作，不能通过新建副本取代它。

### 3. 放入构建材料

从已发布 GitHub Release 或对应 Actions artifact 下载 `SHTMCL_<版本>_aur_x86_64.recipe.tar.gz`，解压到这个 AUR 仓库：

```bash
tar -xzf /path/to/SHTMCL_0.1.0_aur_x86_64.recipe.tar.gz
```

压缩包内只有：

```text
PKGBUILD
.SRCINFO
LICENSE
```

检查下载地址、版本、依赖、许可证及 checksum，并将 PKGBUILD 顶部 Maintainer 注释改为你的 AUR 名称和公开联系方式。

### 4. 本机验证

在普通用户下执行：

```bash
makepkg --printsrcinfo > .SRCINFO
makepkg -s
namcap PKGBUILD shtmcl-bin-*.pkg.tar.zst
```

需要本机安装试用时可以执行 `makepkg -si`。程序通过 pacman 安装到 `/usr`，游戏与缓存继续使用当前用户的数据目录。

### 5. 提交并推送

```bash
git add PKGBUILD .SRCINFO LICENSE
git commit -m "Initial release: 0.1.0-1"
git push origin master
```

推送通过后，AUR 页面即可查询到包。AUR helper 会读取这些构建材料，由用户本机构建和安装。这个流程以 [Arch 官方提交准则](https://wiki.archlinux.org/title/AUR_submission_guidelines)、[PKGBUILD 手册](https://man.archlinux.org/man/PKGBUILD.5.en) 和 [makepkg 手册](https://man.archlinux.org/man/makepkg.8.en) 为依据。

## 后续更新

上游程序出新版本时，先公开新的 GitHub Release，再用新 workflow 生成的三份文件更新 AUR 仓库。`pkgver` 跟随上游版本，`pkgrel` 通常重置为 1。

同一上游版本只修安装路径、依赖等打包问题时，增加 `pkgrel`，例如从 `0.1.0-1` 变为 `0.1.0-2`。本项目模板的 `pkgrel` 可以直接调整；制品收集保留 makepkg 生成的真实包文件名。

手动修改下载源或版本时，可以用 `updpkgsums` 更新摘要；任何元数据更改后重新生成 `.SRCINFO`：

```bash
updpkgsums
makepkg --printsrcinfo > .SRCINFO
makepkg -s
namcap PKGBUILD shtmcl-bin-*.pkg.tar.zst
git add PKGBUILD .SRCINFO LICENSE
git commit -m "Update to 0.1.1-1"
git push origin master
```

后续若要自动同步 AUR，需另行配置专用 AUR SSH 身份，并在公开上游 Release 后更新 AUR；当前流水线保留人工审核和提交。

## 本地生成本项目的 AUR 材料

在 Arch Linux 项目根目录，已有本次构建的 `release-assets/` 时：

```bash
GITHUB_REPOSITORY=你的GitHub用户名/仓库名 \
  node scripts/generate-aur.mjs release-assets
bash scripts/build-aur.sh
```

输出为 `target/aur/shtmcl-bin/`。生成器要求新的空输出目录，防止混入旧版本。此目录还含供本地验证的 deb 缓存和生成的安装包；提交 AUR 时只取前述三份文件。

本轮 Arch 验证记录在 `work/validation/00/aur-packaging-20261110-01/`。由于主项目尚未指定仓库地址，本地测试使用的 `shtmcl-validation/shtmcl` 是明确的验证占位地址，不能用于发布；GitHub Actions 将自动替换为真实仓库。
