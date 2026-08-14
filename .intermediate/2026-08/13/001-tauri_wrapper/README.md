# dsh-desktop：用 Tauri 包装 dsh 的桌面端实施方案

> 目标：双击桌面应用即启动 dsh（`@deepseek-ai/dsh`），并具备对 dsh 本体（npm 包）的自动/手动更新能力。
>
> 规划日期：2026-08-13 · 依据 dsh `0.1.0-rc.6`（registry `latest`）调研

## 一句话方案

**Tauri 2 桌面壳（Rust）+ 受管 dsh 运行时（应用数据目录下的版本化 npm 安装）+ WebView 指向 dsh 自带的本地 Web 服务（`http://127.0.0.1:<port>`）+ 自研 dsh 更新器（registry 检测 + 蓝绿安装 + 原子切换 + 重启生效）。**

## 核心结论（调研事实）

| # | 结论 | 对方案的约束 |
|---|------|-------------|
| 1 | `dsh web` 是 Node ESM CLI，默认监听 `127.0.0.1:3080`，支持 `--port 0`（OS 分配空闲端口），启动后 stdout 打印 `dsh web: http://127.0.0.1:<port>` 握手行 | 启动协议 = 以 `--port 0` 启动 + 解析握手行取真实 URL，天然规避端口冲突 |
| 2 | Web 前端由 dsh 服务端注入 `window.__DSH_BOOT__`，独立打包的前端无法工作 | **WebView 必须加载 dsh 本地服务 URL，不能嵌入 apps/web 静态构建** |
| 3 | `@deepseek-ai/dsh` 无内置自更新机制；registry `latest`/`next` 均为 `0.1.0-rc.6` | 更新能力必须由桌面壳实现（npm registry 为更新源） |
| 4 | dsh 会话按 cwd 分区存储于 `~/.dsh/sessions/<cwd>/` | 桌面端以"打开的文件夹"作为 dsh 进程 cwd，天然获得"每项目一个会话"的体验 |
| 5 | 插件管理依赖 pnpm（`dsh plugin` 转发到 pnpm）；bundle 解析优先安装自身 node_modules | 运行时需 Node；插件体验完整保留（pnpm 由系统提供或随 Node 分发） |
| 6 | 本机 `~/.npm` 缓存被 root 文件污染，`npm view/install` 直接 EPERM | 安装与更新**必须使用独立 npm 缓存目录**（`--cache <appdata>/npm-cache`） |
| 7 | 当前机器：macOS + Node v24.16.0 (nvm) + npm 11.13 + pnpm 11.17 | 一期以系统 Node 起步；二期可捆绑 Node 分发实现自包含 |
| 8 | 实测：dsh spawn→握手 0.86–1.07s；冷 npm 安装 60.2s/334MB；暖 npx 因缓存损坏直接失败 | 启动秒级；bootstrap 需 60s+ 进度条；绝不依赖用户 npm 缓存 |

## 方案结构

```
dsh-desktop/                         # 本仓库
├─ apps/desktop/                     # Tauri 桌面壳
│  ├─ src-tauri/                     # Rust：进程监督 + 运行时管理 + 更新器
│  └─ ui/                            # 壳自带 UI（启动页/错误页/设置页），Vite + TS
└─ .intermediate/…/001-tauri_wrapper # 本文档目录
```

运行时目录（应用数据目录，macOS 为 `~/Library/Application Support/dsh-desktop/`）：

```
dsh-desktop/
├─ runtime/
│  ├─ current -> versions/0.1.0-rc.6/   # 原子指针（current.tmp + rename）
│  └─ versions/<ver>/node_modules/…     # 每版本一次 `npm install --prefix`，蓝绿切换
├─ npm-cache/                            # 独立 npm 缓存（规避 ~/.npm EPERM）
├─ logs/                                 # dsh 子进程 stdout/stderr
└─ settings.json                         # 更新通道/自动更新开关/最近打开目录
```

## 关键机制速览

- **启动**：`node <runtime>/node_modules/@deepseek-ai/dsh/lib/bin.js web --port 0`（cwd = 用户选择的项目目录）→ 解析握手行 → WebView 导航到真实 URL；启动期间显示壳自带 splash。
- **监督**：子进程退出即显示错误页（含日志）；退出应用时 SIGTERM → 超时 SIGKILL；单实例 + 托盘。
- **更新**：定时/启动时查询 `https://registry.npmjs.org/@deepseek-ai%2Fdsh` → semver 比较（通道 `latest`/`next` 可配）→ 安装到新版本目录并验证 `--version` → 原子翻转 `current` 指针 → 重启子进程并刷新 WebView；保留上一版本用于一键回滚。
- **手动更新**：菜单/托盘"检查更新"；**自动更新**：启动时 + 后台定时（默认开启，可在设置页关闭）。

## 前置验证状态（2026-08-13 已执行）

V1–V5 + X1–X3 全部通过，方案核心假设无一处证伪（详见 [05-风险与待验证项.md §0](05-风险与待验证项.md)）：

- ✅ 信任栅栏对 loopback + 随机端口全链路放行（页面/RPC/WS 均实测）；伪造 Host/Origin/跨站标记全部 403；无需 `--trusted-host`
- ✅ 握手行 `dsh web: http://127.0.0.1:<port>` 格式确认；无 TTY 正常；SIGTERM 清理干净无残留
- ✅ WebKit 风格请求头组合、双实例并发 `--port 0`、WebSocket mux 事件流均正常
- 📊 计时：冷 bootstrap 60.2s/334MB（splash 需进度条）；启动到握手 0.86–1.07s（日常秒开）

## 文档索引

| 文档 | 内容 |
|------|------|
| [01-调研与约束.md](01-调研与约束.md) | 对 dsh 包、CLI、Web 服务、registry 与本机环境的调研实录及推导出的硬约束 |
| [02-总体架构.md](02-总体架构.md) | 进程模型、目录布局、启动/关闭时序、Tauri 壳的模块划分与接口 |
| [03-更新机制设计.md](03-更新机制设计.md) | 版本检测、蓝绿安装、原子切换、回滚、自动/手动策略与失败处理 |
| [04-实施路线图.md](04-实施路线图.md) | 分里程碑任务拆解、每阶段验收标准、工作量估算 |
| [05-风险与待验证项.md](05-风险与待验证项.md) | 技术风险、备选方案、决策记录（ADR）与上机验证清单 |

## 分期交付（详见 04）

- **M1 可跑通**（2–3d）：Tauri 壳 + 系统 Node + `npx @deepseek-ai/dsh web --port 0`，WebView 加载成功，退出清理干净。**此阶段已能交付 MVP。**
- **M2 受管运行时**（2d）：应用数据目录内的版本化安装 + 指针 + 设置/日志。
- **M3 更新系统**（2–3d）：自动/手动更新、蓝绿安装、回滚、设置页。
- **M4 打磨**（1–2d）：单实例、托盘、"打开文件夹"、崩溃恢复、错误页。
- **M5 分发**（可选，2d+）：捆绑 Node sidecar 自包含、壳自更新（tauri-plugin-updater）、macOS 签名公证。
