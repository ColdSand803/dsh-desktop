# DSH Desktop

DeepSeek Harness (dsh) 的桌面客户端。用 [Tauri v2](https://tauri.app) 把 `dsh web` 包成一个原生 Windows 应用：

- **图标**：应用/任务栏/托盘统一使用 DeepSeek 官方鲸鱼 logo（取自 DSH 前端 favicon，即折叠侧边栏的图标）
- **顶栏跟随主题**：注入 WebView 脚本监测页面明暗主题并实时同步窗口标题栏——
  深色主题下标题栏为 `#0f1115` 深色、浅色主题下为 `#f9fafb` 浅色，随 dsh 主题切换联动
  （Windows 10 上为模式级明暗跟随；精确 RGB 染色需 Windows 11 的 DWM 扩展属性）
- **无黑窗口**：spawn 后端与清理进程时都带 `CREATE_NO_WINDOW`，全程无命令行窗口闪现
- **托盘常驻**：系统托盘显示鲸鱼图标，左键单击唤出窗口；关闭窗口 = 最小化到托盘（后端继续运行）；托盘菜单「退出」彻底退出并清理后端
- 启动时自动在后台拉起 `dsh web --port 0`（系统自动分配空闲端口），解析后端输出把窗口导航到实际地址
- 退出时 `taskkill /T /F` 杀掉整个后端进程树（含 cloudflared 等辅助进程），后端日志写入 `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log`
- 单实例：重复启动只会聚焦已有窗口

## 环境要求

| 依赖 | 说明 |
|---|---|
| Rust toolchain | `rustup` 安装 stable（>= 1.77） |
| WebView2 Runtime | Win10/11 一般已内置（`EdgeUpdate` 可查版本） |
| dsh | `npm i -g @deepseek-ai/dsh`，`dsh` 需在 PATH 中 |
| Node.js + pnpm/npm | 仅用于 Tauri CLI |

## 开发

```bash
npm install          # 安装 @tauri-apps/cli 与图标渲染工具
node scripts/gen-icons.js   # （可选）从 DSH 前端 favicon 重新生成应用/托盘图标
npm run dev          # 开发模式：编译并弹出应用窗口（首次编译较慢）
```

> 注意：`npm run dev` 与 `cargo build` 目录不同——Tauri CLI 会把工作目录切到 `src-tauri`，所以用 `npm run dev` 而不是在 `src-tauri` 里直接跑 `cargo run`。

## 构建安装包

```bash
npm run build        # 产出 src-tauri/target/release 下的 exe 和 NSIS 安装器
npm run build:no-bundle   # 只编译 exe，不打包安装器
```

## 行为细节

- **工作目录**：进程的工作目录默认取 `USERPROFILE`（dsh 会把运行目录当作默认 workspace 根目录）。
  可用环境变量 `DSH_DESKTOP_WORKDIR` 覆盖，例如：

  ```powershell
  $env:DSH_DESKTOP_WORKDIR = 'D:\dev\my-workspace'
  npm run dev
  ```

- **端口**：始终让 `dsh web --port 0` 自己挑空闲端口，避免与已在 3080 运行的浏览器版冲突——桌面端和浏览器版可以同时开。

- **关窗与退出**：点窗口关闭按钮会把窗口隐藏到托盘（后端继续服务），这是桌面常驻应用的常规行为。
  要真正结束，请用托盘右键菜单的「退出」，或在托盘图标上左键唤回窗口。若希望「关窗即完全退出」，
  把 `main.rs` 中 `RunEvent::WindowEvent ... CloseRequested` 分支删掉即可恢复默认行为。

- **后端生命周期**：后端随应用启动、随应用退出。若 `dsh web` 启动失败（例如 dsh 不在 PATH），
  窗口会停在启动页并自动退出，详细原因见 `dsh-backend.log`。

## 图标生成

图标源是 DSH 前端包内的 `favicon.svg`（DeepSeek 鲸鱼，同折叠侧边栏的图标）：

- `app-icon.png` —— 应用图标（1024×1024，品牌蓝渐变圆角底 + 白色鲸鱼），喂给 `npx tauri icon` 生成全套尺寸与 `.ico`
- `src-tauri/icons/tray.png` —— 托盘图标（透明底 + 品牌蓝鲸鱼，Windows 托盘浅色背景下可见）

重新生成：`node scripts/gen-icons.js` 后执行 `npx tauri icon app-icon.png` 刷新全套图标。

## 目录结构

```
dsh-desktop/
├── ui/                  # 启动占位页（窗口导航到真实 GUI 前显示）
├── scripts/
│   └── gen-icons.js     # 从 DSH favicon.svg 渲染 DeepSeek 鲸鱼图标（应用 + 托盘）
├── src-tauri/
│   ├── src/main.rs      # 核心：拉起后端 / 解析端口 / 导航窗口 / 托盘 / 退出清理
│   ├── Cargo.toml
│   ├── tauri.conf.json  # 窗口与打包配置
│   └── icons/           # npx tauri icon 生成的全套图标 + tray.png
├── app-icon.png         # 应用图标源文件（1024x1024）
└── package.json
```

## 已知限制

- 未配置开机自启、快捷键等；要加的话在 `main.rs` 和 `tauri.conf.json` 里扩展即可。
- 未使用 macOS/Linux 深度适配（代码里做了 `sh -c` 分支，理论上可跨平台，但只在 Windows 验证过）。