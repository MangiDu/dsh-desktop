# dsh-desktop

Tauri 桌面壳，把 [dsh](https://www.npmjs.com/package/@deepseek-ai/dsh)（DeepSeek Harness）包装成双击即用的桌面应用，并具备对 dsh 运行时（`@deepseek-ai/dsh` npm 包）的自动/手动更新能力。

> 实施方案见 [`.intermediate/2026-08/13/001-tauri_wrapper/`](.intermediate/2026-08/13/001-tauri_wrapper/README.md)。

## 架构

```
Tauri 2 壳 (Rust)
 ├─ spawn: node <dsh>/lib/bin.js web --port 0   (cwd = 项目目录, M4 可选)
 ├─ 解析 stdout 握手行 "dsh web: http://127.0.0.1:<port>"
 ├─ 主 WebView ──► http://127.0.0.1:<port>        (dsh 自带 Web GUI, 注入 __DSH_BOOT__)
 └─ 壳 UI (splash / 错误页 / 设置页)              (Vite, tauri://localhost)
```

- dsh 以 `--port 0` 启动（OS 分配空闲端口，无冲突）；前端必须由 dsh 服务端提供，壳不打包前端。
- 退出时 SIGTERM → 3s 宽限 → SIGKILL；启动时回收孤儿进程（pidfile + `ps -E` 标记校验）。
- M2 已完成：dsh 安装到应用数据目录（版本化 + `current` 指针 + 首次自动 bootstrap）。
- 关闭按钮二次确认：完全退出 / 后台运行（隐藏窗口，点击 Dock 图标恢复）。
- 非侵入事件监听（`listener.rs`）：壳以**额外消费者**身份接入 dsh 公开下行流 `/api/events.mux` 与 `/api/events.host`（WebSocket，纯只读——服务器在收到任何上行消息时会主动关闭连接）。host 向所有已连接消费者扇出事件帧，并在新连接打开时**重放未决条目**，因此壳的监听不改变 GUI 的任何行为，也不修改 dsh 的任何代码或内部状态——只消费公开协议。`approval/requested`、`question/requested`、`host/agent-error` 等事件映射为系统通知（macOS 使用消息中心横幅，**不显示自定义 Toast**）+ Dock 徽标；其他平台使用应用内 Toast 横幅（锚定主窗口所在显示器右上角，审批/提问类常驻直至点击或请求解决）。
- macOS 交互加固：`acceptsFirstMouse` 全局置 YES（非 key 窗口首次点击直达，浏览器式行为）；任何菜单结束跟踪 / 应用重新激活后把 key 还给主窗口；WKWebView 禁用压力手势。另注入**点击补偿器**（初始化脚本）：dsh 侧栏在 mousedown 瞬间会滚动（条目聚焦/选中触发），光标下节点被换、WebKit 丢弃 click——补偿器在按下后 90ms 内未收到 click、位移 <6px、原节点仍在文档时补发一次 click（真实拖拽/双击/右键不受影响），保证侧栏工作区/会话快速来回点击一次触达。
- 应用菜单：**检查更新…**（registry 检测 → 询问 → 蓝绿安装 → 询问重启）、**安装插件…**（shell UI 面板，执行 `dsh plugin --profile web add <pkg>`，日志实时流式显示）、**重启 dsh**、退出。
- 插件离线安装：zip 包 / 目录本地安装（`add <本地路径> --offline`，内网可用）。
- M3 已完成：自动更新调度器（autoUpdate + intervalHours + lastCheck 防抖）、版本保留（设置面板可配 N，默认 1、最大 10）、更新面板版本列表一键切换、新版本启动失败自动回滚（lastKnownGood）、更新历史记录。
- M4 打磨：单实例、崩溃恢复（连续失败≥3 可重置运行时）、日志目录入口已就绪。**「打开文件夹」经评估不实施**：dsh 自带工作区概念（GUI 会话列表按目录分组 + 内建原生目录选择器），工作区切换完全交给 dsh GUI；`~/dsh-desktop-workspace` 仅作为桌面子进程的默认工作区锚点（DSH_CWD 可覆盖）。
- M5 分发。

## 开发

```bash
pnpm install
pnpm desktop:dev        # tauri dev（需系统 Node；dsh 路径经 DSH_BIN 或默认值解析）
```

环境要求：Rust 1.97+、Node ≥ 22、pnpm 11。可选环境变量：

| 变量 | 作用 |
|------|------|
| `DSH_BIN` | `@deepseek-ai/dsh` 的 `lib/bin.js` 绝对路径（M2 起被受管运行时取代） |
| `DSH_CWD` | dsh 子进程工作目录（默认继承壳的 cwd；会话按 cwd 分区） |

本仓库把 cargo 缓存（`.cargo-home/`）与 pnpm store（`.pnpm-store/`）放在仓库内，规避用户级 npm 缓存问题（本机 `~/.npm` 损坏）与文件沙箱限制。

## macOS 通知：替换构建后的授权与手动刷新

macOS 把通知授权绑定在应用的**代码签名身份**上（`Identifier` + `CDHash` + 绑定进签名的 `Info.plist`），而不是应用名字：

- 打包必须做正规 bundle 签名（已配置 `bundle.macOS.signingIdentity: "-"`）。否则签名身份与 `CFBundleIdentifier` 不一致（曾出现 `Identifier=dsh_desktop-<hash>`、`Info.plist=not bound`），`usernoted` 无法识别应用，授权请求被直接拒绝（`UNErrorDomain error 1`：Notifications are not allowed for this application）且**永不弹出授权对话框**。
- 当前是 ad-hoc 签名，`CDHash` 每次构建都会变，所以**每替换一次构建，通知授权就失效一次**——正常情况下替换后**首次启动即弹出授权对话框**（系统无记录 → 主动询问），点「允许」即可，无需其他操作。
- 仅当启动后**没有**弹出授权提示时（通常发生在多次替换、`usernoted`/LaunchServices 残留旧记录时），运行脚本刷新：

  ```bash
  pnpm notify:refresh    # = bash scripts/refresh-notifications.sh [app路径]
  ```

- 一劳永逸的方案是 Developer ID 签名 + 公证（M5-3，需要 Apple 开发者账号）。在此之前，应用数据目录 `logs/notify.log` 会记录每次事件的授权状态与错误（`sysnotify: status=…`、`granted=… err=…`），排查通知问题时先看它。

## 目录

```
apps/desktop/           Tauri 壳
  src-tauri/src/        Rust: dsh.rs (进程监督) / lib.rs (窗口与命令)
  src/                  shell UI: splash / 错误页 / 重试
scripts/                refresh-notifications.sh（替换构建后刷新 macOS 通知身份）
.intermediate/…/        规划与验证文档
```

## 里程碑

- [x] 前置验证（信任栅栏/握手行/计时/清理，2026-08-13）
- [x] M0 脚手架 + M1 MVP（2026-08-13/14：spawn `dsh web --port 0` → 握手解析 → 主 WebView；孤儿回收；错误页+重试；退出无残留）
- [x] M2 受管运行时（2026-08-14：版本化安装 + `current` 指针 + 首次 bootstrap，二次启动 4.2s 无重装）
- [x] 补充需求：关闭确认（退出/后台）+ 非侵入事件通知（mux/host 监听 + 原生通知）
- [x] 补充需求：手工检查更新菜单（检测→询问→蓝绿安装→询问重启）+ 插件安装面板（`dsh plugin` 命令入口）
- [x] M3 更新系统（2026-08-14：调度器/版本保留/回滚/自动回滚/历史，演练 A+B 通过）
- [x] M4 打磨（2026-08-14：单实例/崩溃恢复/日志入口；打开文件夹经评估由 dsh 内建工作区覆盖，不实施）
- [ ] M5 分发
