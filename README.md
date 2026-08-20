# DSH Desktop

DeepSeek Harness (dsh) 的桌面客户端。用 [Tauri v2](https://tauri.app) 把 `dsh web` 包成一个原生 Windows 应用：

- **图标**：应用/任务栏/托盘统一使用 DeepSeek 官方鲸鱼 logo（取自 DSH 前端 favicon，即折叠侧边栏的图标）
- **顶栏跟随主题**：注入 WebView 脚本监测页面明暗主题并实时同步窗口标题栏——
  深色主题下标题栏为 `#0f1115` 深色、浅色主题下为 `#f9fafb` 浅色，随 dsh 主题切换联动
  （Windows 10 上为模式级明暗跟随；精确 RGB 染色需 Windows 11 的 DWM 扩展属性）
- **无黑窗口**：spawn 后端与清理进程时都带 `CREATE_NO_WINDOW`，全程无命令行窗口闪现
- **托盘常驻**：系统托盘显示鲸鱼图标，左键单击唤出窗口；关闭窗口 = 最小化到托盘（后端继续运行）；托盘菜单「退出」彻底退出并清理后端
- 启动时自动在后台拉起 `dsh web --port 0`（系统自动分配空闲端口），解析后端输出把窗口导航到实际地址
- **没装 dsh 也能用**：启动前先探测环境，缺 `dsh` 时窗口停在引导页，可一键 `npm i -g @deepseek-ai/dsh`（日志实时显示），装完自动接着启动；连 npm 都没有则引导去装 Node.js
- 退出时结束整个后端进程树（含 cloudflared 等辅助进程），Windows 上直接 `taskkill /T /F`。这里没有「先礼后兵」的余地：后端是个无窗口的 `cmd` 套 node，普通 `taskkill /T` 对树里每个进程都只回一句「只能强制终止此任务(带 /F 选项)」，等它等不出结果，只会拖长退出——而退出没完成前单实例锁不释放，「退出后立刻重开」会静默失效。留下的残留由下次启动时的锁清理兜住。后端日志写入 `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log`
- 单实例：重复启动只会聚焦已有窗口

## 环境要求

| 依赖 | 说明 |
|---|---|
| Rust toolchain | `rustup` 安装 stable（>= 1.77） |
| WebView2 Runtime | Win10/11 一般已内置（`EdgeUpdate` 可查版本） |
| dsh | **可选**——没装时应用内可一键安装（`npm i -g @deepseek-ai/dsh`）；已装则需在 PATH 中 |
| Node.js + pnpm/npm | Tauri CLI 需要；`npm` 同时也是应用内一键安装 dsh 的前提 |

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

- **后端生命周期**：后端随应用启动、随应用退出。后端自己退出时应用也会跟着退出，不会停在死页面上。

- **没装 dsh 时的引导**：启动前会先探测 `dsh` 是否在 PATH 中，结果分三种——

  | 情况 | 页面表现 |
  |---|---|
  | 有 `dsh` | 正常启动，直接进 GUI |
  | 没 `dsh`、有 `npm` | 说明页 + 「一键安装」按钮，点了就跑 `npm install -g @deepseek-ai/dsh`，日志实时滚动；装完自动接着启动（npm 全局目录本来就在 PATH 上，不用重启） |
  | 连 `npm` 都没有 | 提示先装 Node.js，带一个「打开 nodejs.org」按钮 |

  首次运行不需要手动初始化 profile——`dsh web` 在全新的 `DSH_HOME` 下会自己把 profile 装起来。

- **启动失败**：不再闪退。窗口会停在错误页，把后端日志里最相关的几行直接显示出来（优先显示指名道姓的
  `Cannot find ...` 这类，而不是外层笼统的「plugin tree failed to load」），并提供「重试」按钮。
  完整日志仍在 `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log`（每次启动覆写）。

## 图标生成

图标源是 DSH 前端包内的 `favicon.svg`（DeepSeek 鲸鱼，同折叠侧边栏的图标）：

- `app-icon.png` —— 应用图标（1024×1024，品牌蓝渐变圆角底 + 白色鲸鱼），喂给 `npx tauri icon` 生成全套尺寸与 `.ico`
- `src-tauri/icons/tray.png` —— 托盘图标（透明底 + 品牌蓝鲸鱼，Windows 托盘浅色背景下可见）

重新生成：`node scripts/gen-icons.js` 后执行 `npx tauri icon app-icon.png` 刷新全套图标。

## 目录结构

```
dsh-desktop/
├── ui/                  # 启动 / 引导页（状态机：启动中、缺 dsh、安装中、缺 Node、出错）
├── scripts/
│   └── gen-icons.js     # 从 DSH favicon.svg 渲染 DeepSeek 鲸鱼图标（应用 + 托盘）
├── src-tauri/
│   ├── src/main.rs      # 核心：环境探测 / 一键安装 / 拉起后端 / 导航窗口 / 托盘 / 退出清理
│   ├── Cargo.toml
│   ├── tauri.conf.json  # 窗口与打包配置
│   └── icons/           # npx tauri icon 生成的全套图标 + tray.png
├── app-icon.png         # 应用图标源文件（1024x1024）
└── package.json
```

## 已知限制

- 未配置开机自启、快捷键等；要加的话在 `main.rs` 和 `tauri.conf.json` 里扩展即可。
- 未使用 macOS/Linux 深度适配（代码里做了 `sh -c` 分支，理论上可跨平台，但只在 Windows 验证过）。
- 一键安装走的是系统默认 npm registry，公司内网/需要代理的环境可能装不动——这种情况下按页面上给的
  命令自己在终端里装（可以先配好 registry 或代理）。
- Windows 下退出必然是强杀（原因见上），所以**正在装插件时退出应用有损坏 dsh 插件目录的风险**：dsh 用 pnpm 换包时是「先清空目标目录、再把 `<pkg>_tmp_...` 改名盖上去」，强杀正好落在这中间，那个包就只剩一个没有 `package.json` 的 `src/`，下次启动报 `ERR_MODULE_NOT_FOUND`，得重装该包才能恢复。要根治得让后端能优雅退出（Windows 上的正路是 Job Object，或给 `cmd` 发 CTRL_BREAK），目前没做。装插件时请等它装完再退出。
- Windows 下 `cmd` 自身报的错（比如「不是内部或外部命令」）用的是控制台 OEM 代码页，落进日志会是乱码。
  加了启动前探测之后基本不会再触发这条路径，暂时没有引入编码转换依赖。