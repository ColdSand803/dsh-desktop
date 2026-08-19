# DSH Desktop

DeepSeek Harness (dsh) 的桌面客户端。用 [Tauri v2](https://tauri.app) 把 `dsh web` 包成一个原生 Windows 应用：

- **图标**：应用/任务栏/托盘统一使用 DeepSeek 官方鲸鱼 logo（取自 DSH 前端 favicon，即折叠侧边栏的图标）
- **顶栏跟随主题**：注入 WebView 脚本监测页面明暗主题并实时同步窗口标题栏——
  深色主题下标题栏为 `#0f1115` 深色、浅色主题下为 `#f9fafb` 浅色，随 dsh 主题切换联动
  （Windows 10 上为模式级明暗跟随；精确 RGB 染色需 Windows 11 的 DWM 扩展属性）
- **无黑窗口**：spawn 后端与清理进程时都带 `CREATE_NO_WINDOW`，全程无命令行窗口闪现
- **托盘常驻**：系统托盘显示鲸鱼图标，左键单击唤出窗口；关闭窗口 = 最小化到托盘（后端继续运行）；托盘菜单「退出」彻底退出并清理后端
- 启动时先探测 3080 上是否已有 dsh web：有就复用，没有才自己拉起 `dsh web --port 0`（系统分配空闲端口），
  解析后端输出把窗口导航到实际地址
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

## 发布

打 tag 触发 `.github/workflows/release.yml`：构建、签名、建一个 **draft** release，
把安装器和 `latest.json` 传上去。确认无误后手动 publish。

```bash
# tag 里的版本必须和 tauri.conf.json 的 version 一致，
# 否则 updater 不会认为这个 release 比用户手上的新。
git tag v0.1.0 && git push --tags
```

### 首次发布前必须配的 secret

自动更新靠签名验证，**没有签名的更新会被拒绝**。密钥对已经生成好了，公钥在
`tauri.conf.json` 的 `plugins.updater.pubkey`，私钥在本地 `~/.tauri/dsh-desktop.key`
（不在仓库里，`.gitignore` 已覆盖 `*.key` 和 `.tauri/`）。

在 GitHub 仓库 Settings → Secrets and variables → Actions 里加两个：

| Secret | 值 |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | `~/.tauri/dsh-desktop.key` 的**文件内容** |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 空字符串（生成时没设密码） |

> 私钥要备份好。**丢了就再也无法给已安装的用户推更新**，只能让他们手动重装；
> 泄漏了则任何人都能给你的用户推任意更新。

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

- **关窗与退出**：点窗口关闭按钮会把窗口隐藏到托盘（后端继续服务），这是桌面常驻应用的常规行为。
  要真正结束，请用托盘右键菜单的「退出」，或在托盘图标上左键唤回窗口。若希望「关窗即完全退出」，
  把 `main.rs` 中 `RunEvent::WindowEvent ... CloseRequested` 分支删掉即可恢复默认行为。

- **后端生命周期**：后端随应用启动、随应用退出。若 `dsh web` 启动失败（例如 dsh 不在 PATH），
  窗口会留在启动页并把失败原因直接显示出来（不再自动退出，否则窗口一闪就没了、看不到原因）；
  修好后从托盘「退出」再重开。完整输出见 `dsh-backend.log`。

- **后端日志**：stdout 和 stderr 都写进 `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log`，
  分别以 `[out]` / `[err]` 前缀区分。超过 5 MB 会滚动到 `dsh-backend.log.1`（只保留一份）。

- **进程清理**：后端子进程被放进 Windows Job Object（`KILL_ON_JOB_CLOSE`），所以即使桌面端
  异常终止（panic、任务管理器结束、注销），`dsh web` 整个进程树也会被系统连带回收。

## 图标生成

图标源是 DSH 前端包内的 `favicon.svg`（DeepSeek 鲸鱼，同折叠侧边栏的图标）：

- `app-icon.png` —— 应用图标（1024×1024，品牌蓝渐变圆角底 + 白色鲸鱼），喂给 `npx tauri icon` 生成全套尺寸与 `.ico`
- `src-tauri/icons/tray.png` —— 托盘图标（透明底 + 品牌蓝鲸鱼，Windows 托盘浅色背景下可见）

重新生成：`node scripts/gen-icons.js` 后执行 `npx tauri icon app-icon.png` 刷新全套图标。

脚本会自己找 `favicon.svg`：先 `require.resolve`（dsh 装在本地时），再查 `npm root -g` 下
dsh 的全局安装位置。都找不到会报错并列出试过的路径。也可以手动指定：

```powershell
$env:DSH_FAVICON = 'C:\path\to\favicon.svg'
node scripts/gen-icons.js
```

## 目录结构

```
dsh-desktop/
├── .github/workflows/
│   └── ci.yml           # windows-latest 上跑 fmt / clippy / test
├── ui/                  # 启动占位页（窗口导航到真实 GUI 前显示；也承载启动失败的错误态）
├── scripts/
│   └── gen-icons.js     # 从 DSH favicon.svg 渲染 DeepSeek 鲸鱼图标（应用 + 托盘）
├── src-tauri/
│   ├── src/main.rs      # 核心：拉起后端 / 解析端口 / 导航窗口 / 托盘 / 退出清理
│   ├── capabilities/
│   │   ├── default.json      # 本地页面（启动占位页）的权限
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
- 标题栏精确染色需要 Windows 11：`DWMWA_CAPTION_COLOR` / `TEXT_COLOR` / `BORDER_COLOR` 要 build 22000+，
  Win10 上这几个调用会无害失败，只剩 `USE_IMMERSIVE_DARK_MODE` 生效（模式级明暗跟随）。
- 未使用 macOS/Linux 深度适配（代码里做了 `sh -c` 分支，理论上可跨平台，但只在 Windows 验证过）。
  Job Object 清理是 Windows 专有的，其他平台只有退出时的 `kill -9`。
- 依赖 `dsh` 的 **stdout 格式**：靠解析 `dsh web: http://127.0.0.1:<port>` 拿地址。
  dsh 目前是 developer preview（`0.1.0-rc.7`，README 明说会有破坏性变更），
  哪天改了输出格式，导航会失败并停在启动页的错误态上。

## License

[MIT](LICENSE) © ColdSand803

图标源自 DeepSeek Harness 前端的 `favicon.svg`（[deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)，MIT）。