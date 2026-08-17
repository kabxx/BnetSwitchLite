# 开发者文档

本文面向参与开发、测试、构建和发布 BnetSwitchLite 的人员。普通用户请先阅读项目根目录的 [README](../README.md)。

## 项目概览

BnetSwitchLite 使用 React + TypeScript 构建界面，使用 Tauri 将前端操作桥接到 Rust 服务。平台相关代码位于 `src-tauri/src/platform`，Windows 和 macOS 的进程控制、路径解析、快照恢复和安全存储相互隔离。

主要目录：

```text
src/                         React 界面、组件和前端控制器
src-tauri/src/               Tauri 命令、数据模型和平台服务
src-tauri/src/platform/      Windows/macOS 平台实现
src-tauri/icons/             应用图标资源
scripts/                     发布产物打包脚本
docs/                        开发者文档和 README 预览图
```

前端调用路径大致为：

```text
React 组件 -> use-app-controller -> bridge -> Tauri commands -> platform service
```

涉及账号识别、客户端进程控制、登录状态、快照恢复和数据存储的改动，应优先保持这条边界，不要让 React 直接承担文件恢复或进程控制逻辑。

## 开发环境

- Node.js 24，使用仓库锁定的 npm 依赖
- Rust 1.85 或更高版本
- Windows：Visual Studio C++ Build Tools 和 MSVC 工具链
- macOS：Apple Command Line Tools、Darwin Rust targets，以及签名/公证所需的证书和凭据
- Microsoft Edge WebView2 Runtime（Windows 运行和测试需要）

安装前端依赖：

```powershell
npm ci
```

## 本地开发

Tauri 开发模式会同时启动 Vite 和桌面应用：

```powershell
npm run tauri -- dev
```

只需要检查前端编译时可以运行：

```powershell
npm run dev
```

但纯 Vite 页面没有 Tauri IPC，账号读取、客户端控制等桌面功能不能在该模式下使用。

## 检查与测试

提交前至少运行：

```powershell
npm run lint
npm run typecheck
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --lib --locked
```

如果只修改了前端，可以先运行前三项；修改 Rust、平台逻辑、数据格式或恢复流程时，应运行完整检查。

## 版本管理

发布版本必须统一为同一个 SemVer 版本号，至少同步以下文件：

- `package.json`
- `package-lock.json` 根包信息
- `src-tauri/Cargo.toml`
- `src-tauri/Cargo.lock` 根包信息
- `src-tauri/tauri.conf.json`

Windows 打包脚本会读取 `package.json` 的版本，并按架构生成 `release/BnetSwitchLite-<version>-windows-x64.exe` 或 `release/BnetSwitchLite-<version>-windows-arm64.exe`。不要手动上传旧版本产物。

## Windows 构建和打包

构建 Windows x64 原始便携 EXE：

```powershell
npm run tauri -- build --no-bundle --ci
```

生成 x64 发布副本并校验 SHA-256：

```powershell
npm run package:windows -- -Architecture x64
```

构建 Windows ARM64 原始便携 EXE：

```powershell
rustup target add aarch64-pc-windows-msvc
npm run tauri -- build --target aarch64-pc-windows-msvc --no-bundle --ci
```

生成 ARM64 发布副本并校验 SHA-256：

```powershell
npm run package:windows -- -Architecture arm64
```

脚本要求对应 target 的 `BnetSwitchLite.exe` 存在，并拒绝 EXE 旁边出现 DLL。不要将 `target` 中的调试符号、PDB、临时数据目录或旧产物提交到仓库。

## macOS 构建和发布检查

macOS 版本必须在真实 Mac 上构建。发布时分别生成 Apple Silicon 和 Intel 两个独立包，不把两个架构塞进同一个下载文件。构建前需要设置已核验的 Blizzard/Battle.net Team Identifier：

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
export BNETSWITCHLITE_BLIZZARD_TEAM_ID='<verified-team-id>'
npm run bundle:macos:arm64
npm run package:macos -- 1.0.0 arm64
npm run bundle:macos:x64
npm run package:macos -- 1.0.0 x64
```

正式签名版需要在构建环境中提供 Developer ID Application 证书；签名、公证凭据只放在构建环境中，不要写入仓库。若发布环境已经完成公证，还应设置 `BNETSWITCHLITE_REQUIRE_NOTARIZATION=1`，让打包脚本验证票据。

发布前至少检查：

```bash
file src-tauri/target/aarch64-apple-darwin/release/bundle/macos/BnetSwitchLite.app/Contents/MacOS/BnetSwitchLite
codesign --verify --deep --strict --verbose=2 src-tauri/target/aarch64-apple-darwin/release/bundle/macos/BnetSwitchLite.app
spctl --assess --type execute --verbose=4 src-tauri/target/aarch64-apple-darwin/release/bundle/macos/BnetSwitchLite.app
file src-tauri/target/x86_64-apple-darwin/release/bundle/macos/BnetSwitchLite.app/Contents/MacOS/BnetSwitchLite
codesign --verify --deep --strict --verbose=2 src-tauri/target/x86_64-apple-darwin/release/bundle/macos/BnetSwitchLite.app
spctl --assess --type execute --verbose=4 src-tauri/target/x86_64-apple-darwin/release/bundle/macos/BnetSwitchLite.app
```

发布前还要分别实测两个架构的首次启动、升级保留数据、Battle.net 双向切换、登录和失败恢复；未通过这些检查时，不要在 README 中宣称对应架构已正式支持。

## 依赖许可证

项目使用 MIT License。`NOTICE` 用于记录 Rust 第三方依赖的许可证与归属信息，不用于声明项目来源。依赖变化后应重新生成或核对对应的 Rust 依赖清单，并保留依赖要求的版权和许可证文本。

## 数据与安全边界

不要把以下内容提交到仓库：

- BattleTag、账号 ID 或其他真实账号信息
- `CachedData.db`、`Battle.net.config` 和任何 Battle.net 数据副本
- `BnetSwitchLiteData/` 或 macOS 应用数据目录
- 登录状态、认证快照和调试转储
- 签名证书、私钥、公证凭据和构建环境密钥

修改恢复事务、登录会话、快照校验或平台文件操作时，应同时补充对应测试，并验证失败路径不会留下可继续使用的半成品状态。
