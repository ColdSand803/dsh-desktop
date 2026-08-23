# DSH Desktop

[English](README.md)

DeepSeek Harness (dsh) 的桌面客户端。用 [Tauri v2](https://tauri.app) 把 `dsh web` 包成一个原生 Windows 应用：

- **图标**：应用/任务栏/托盘统一使用 DeepSeek 官方鲸鱼 logo（取自 DSH 前端 favicon，即折叠侧边栏的图标）
- **标题栏和 dsh 侧边栏同色**：标题栏是自己画的（窗口以 `decorations: false` 创建，没有原生标题栏），
  颜色不是两档硬编码色，而是读 dsh 自己的设计 token `--dsw-specific-sidebar-fill` 取到的实际 RGB。
  换主题（包括第三方主题）会跟着变，**Win10 / Win11 表现一致**
- **无黑窗口**：spawn 后端与清理进程时都带 `CREATE_NO_WINDOW`，全程无命令行窗口闪现
- **托盘常驻**：系统托盘显示鲸鱼图标，左键单击唤出窗口；关闭窗口 = 最小化到托盘（后端继续运行）；托盘菜单「退出」彻底退出并清理后端
- 启动时先探测 3080 上是否已有 dsh web：有就复用，没有才自己拉起 `dsh web --port 0`（系统分配空闲端口），
  解析后端输出，把实际地址交给外壳页的 iframe（窗口本身不导航，见下）
- **没装 dsh 也能用**：启动前先探测环境，缺 `dsh` 时窗口停在引导页，可一键 `npm i -g @deepseek-ai/dsh`（日志实时显示），装完自动接着启动；连 npm 都没有则引导去装 Node.js
- 退出时结束整个后端进程树（含 cloudflared 等辅助进程）。Windows 上只能强杀，原因和实测见
  `main.rs` 里 `kill_process_tree` 的注释
- **异常终止也不残留**：后端子进程被放进 Windows Job Object（`KILL_ON_JOB_CLOSE`），即使桌面端 panic、被任务管理器结束或用户注销，整个 `dsh web` 进程树也会被系统连带回收
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

安装器是 **per-user** 的（`installMode: currentUser`）：装到用户目录、不弹 UAC、注册信息写 `HKCU`。
语言按系统语言在简体中文 / 英文之间自动选，都不匹配则回落到简体中文。

## 测试与 CI

```bash
cd src-tauri
cargo test           # extract_url 的单测
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

`.github/workflows/ci.yml` 在 push / PR 时在 windows-latest 上跑同样这三步。
只跑 Windows：这个应用本来就是 Windows 目标（DWM 标题栏染色、job object、taskkill），
非 Windows 的 cfg 分支不是实际会发布的东西。

## 行为细节

- **工作目录**：进程的工作目录默认取 `USERPROFILE`（dsh 会把运行目录当作默认 workspace 根目录）。
  可用环境变量 `DSH_DESKTOP_WORKDIR` 覆盖，例如：

  ```powershell
  $env:DSH_DESKTOP_WORKDIR = 'D:\dev\my-workspace'
  npm run dev
  ```

- **端口**：启动时先探测 `3080`（探测端口，可用 `DSH_DESKTOP_PROBE_PORT` 覆盖）。

  - 探到已有 dsh web 在跑（比如浏览器那个实例），就**直接复用**它，不再起自己的后端。
    这样可以避开 task-board 的单实例锁互相冲突。这种情况下退出桌面端**不会**杀掉那个后端——它不归我们管。
  - 没探到，才自己拉起 `dsh web --port 0` 让系统挑空闲端口。

- **标题栏 / 外壳结构**：窗口以 `decorations: false` 创建，整个生命周期都停在外壳页
  `ui/index.html` 上、**从不导航**；标题栏由外壳自己画，dsh 的 GUI 装在外壳的 iframe 里。
  这样标题栏才能是任意颜色——原生标题栏的染色属性需要 Windows 11。详细原理见 `main.rs`
  里 `THEME_WATCH_JS` 和窗口创建处的注释。

- **关窗与退出**：点窗口关闭按钮会把窗口隐藏到托盘（后端继续服务），这是桌面常驻应用的常规行为。
  要真正结束，请用托盘右键菜单的「退出」，或在托盘图标上左键唤回窗口。若希望「关窗即完全退出」，
  把 `main.rs` 中 `RunEvent::WindowEvent ... CloseRequested` 分支删掉即可恢复默认行为。

- **后端生命周期**：后端随应用启动、随应用退出。如果后端自己崩了，应用**不会**跟着消失——
  会收起 iframe、显示错误页和「重试」按钮。

- **没装 dsh 时的引导**：启动前会先探测 `dsh` 是否在 PATH 中，结果分三种——

  | 情况 | 页面表现 |
  |---|---|
  | 有 `dsh` | 正常启动，直接进 GUI |
  | 没 `dsh`、有 `npm` | 说明页 + 「一键安装」按钮，点了就跑 `npm install -g @deepseek-ai/dsh`，日志实时滚动；装完自动接着启动（npm 全局目录本来就在 PATH 上，不用重启） |
  | 连 `npm` 都没有 | 提示先装 Node.js，带一个「打开 nodejs.org」按钮 |

  首次运行不需要手动初始化 profile——`dsh web` 在全新的 `DSH_HOME` 下会自己把 profile 装起来。

- **启动失败**：不再闪退。窗口会停在错误页，把后端日志里最相关的几行直接显示出来（优先显示指名道姓的
  `Cannot find ...` 这类，而不是外层笼统的「plugin tree failed to load」），并提供「重试」按钮。
  完整日志仍在 `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log`。

- **后端日志**：stdout 和 stderr 都写进 `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log`，
  分别以 `[out]` / `[err]` 前缀区分。**每次启动覆写**，所以要留证据得在重启前先拷出来；
  单次运行内超过 5 MB 会滚动到 `dsh-backend.log.1`（只保留一份）。

## 目录结构

```
dsh-desktop/
├── .github/workflows/
│   ├── ci.yml           # windows-latest 上跑 fmt / clippy / test
│   └── release.yml      # 打 tag 时构建、签名、建 draft release
├── ui/                  # 外壳页：自绘标题栏 + GUI iframe + 引导状态机
│                        #   （启动中、缺 dsh、安装中、缺 Node、出错、ready）
├── scripts/
│   └── gen-icons.js     # 从 DSH favicon.svg 渲染 DeepSeek 鲸鱼图标（应用 + 托盘）
├── src-tauri/
│   ├── src/main.rs      # 核心：环境探测 / 一键安装 / 拉起后端 / 主题取色 / 托盘 / 退出清理
│   ├── capabilities/
│   │   ├── default.json      # 外壳页的权限（含自绘标题栏要的 core:window:*）
│   │   └── remote-theme.json # 后端 origin 的权限：只给 event:emit，用于主题上报
│   ├── Cargo.toml
│   ├── tauri.conf.json  # 窗口与打包配置
│   └── icons/           # npx tauri icon 生成的全套图标 + tray.png
├── app-icon.png         # 应用图标源文件（1024x1024）
└── package.json
```

## 已知限制

- 未配置开机自启、全局快捷键；要加的话在 `main.rs` 和 `tauri.conf.json` 里扩展即可。
  （这两个都会改变用户可见行为，默认打开不合适，所以留空。）
- 自动更新是**手动触发**的（托盘「检查更新」），不会在启动时自动查。更新会替换正在运行的
  二进制并需要重启，不该在用户不知情的时候发生。
- **GUI 跑在 iframe 里**，这是自绘标题栏的代价。目前可用：dsh 没有设 `X-Frame-Options`
  或 CSP `frame-ancestors`，前端也没有 frame-busting。但这不是它承诺的接口——哪天 dsh 加上
  这类头，GUI 区域就会白屏。
- `DWMWA_CAPTION_COLOR` / `TEXT_COLOR` / `BORDER_COLOR` 仍然要 Windows 11（build 22000+），
  Win10 上无害失败。但这**不影响标题栏颜色**——标题栏是自绘的，不经过 DWM；这几个属性现在只用来
  染窗口边框，以及设 `USE_IMMERSIVE_DARK_MODE`。
- 未使用 macOS/Linux 深度适配（代码里做了 `sh -c` 分支，理论上可跨平台，但只在 Windows 验证过）。
  Job Object 清理是 Windows 专有的，其他平台只有退出时的 `SIGTERM` → `kill -9`。
- 依赖 `dsh` 的 **stdout 格式**：靠解析 `dsh web: http://127.0.0.1:<port>` 拿地址。
  dsh 目前是 developer preview，自己的 README 明说会有破坏性变更——哪天改了输出格式，
  就拿不到地址，会停在外壳页的错误态上。
- 一键安装走的是系统默认 npm registry，公司内网/需要代理的环境可能装不动——这种情况下按页面上给的
  命令自己在终端里装（可以先配好 registry 或代理）。
- Windows 下退出必然是强杀（原因见上），所以**正在装插件时退出应用有损坏 dsh 插件目录的风险**：dsh 用 pnpm 换包时是「先清空目标目录、再把 `<pkg>_tmp_...` 改名盖上去」，强杀正好落在这中间，那个包就只剩一个没有 `package.json` 的 `src/`，下次启动报 `ERR_MODULE_NOT_FOUND`，得重装该包才能恢复。Job Object 只保证「一定收得干净」，不解决「收得优雅」——要根治得让后端能优雅退出（Windows 上的正路是给 `cmd` 发 CTRL_BREAK），目前没做。装插件时请等它装完再退出。
- Windows 下 `cmd` 自身报的错（比如「不是内部或外部命令」）用的是控制台 OEM 代码页，落进日志会是乱码。
  加了启动前探测之后基本不会再触发这条路径，暂时没有引入编码转换依赖。

## 链接

- [发行作品](https://github.com/ColdSand803/dsh-desktop/releases)
- [议题](https://github.com/ColdSand803/dsh-desktop/issues)
- [Linux.do](https://linux.do/) —— 社区讨论

## 参与开发

见 [CONTRIBUTING.md](CONTRIBUTING.md)。安全问题请走 [SECURITY.md](SECURITY.md) 里的私密上报，不要开公开 issue。

## License

[MIT](LICENSE) © ColdSand803

图标派生自 dsh 前端 `favicon.svg` 里的 DeepSeek 鲸鱼（MIT，© 2026 DeepSeek）。dsh 本身不被打包也不再分发——由用户自己用 npm 装，应用只是把它当子进程拉起来。上游的版权声明见 [NOTICE](NOTICE)。