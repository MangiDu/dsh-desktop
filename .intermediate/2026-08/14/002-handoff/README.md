# dsh-desktop 交接文档

> 日期：2026-08-14 · 交接人：上一会话的 AI 代理 · 状态：**可运行、可续做**
> 本仓库位于 `/Users/duxx/workspace/dsh-desktop`，git 共 37+ 提交，工作区干净。
> 上游规划文档：`.intermediate/2026-08/13/001-tauri_wrapper/`（README 为总入口）。

## 1. 项目是什么

Tauri 2 桌面壳，把 [@deepseek-ai/dsh](https://www.npmjs.com/package/@deepseek-ai/dsh)（DeepSeek Harness，`0.1.0-rc.6`）包装成双击即用的桌面应用：自管理 dsh 运行时（版本化 + 更新/回滚）、非侵入事件提示（审批/提问弹横幅）、关闭确认、插件安装（在线/离线）、单实例、崩溃恢复、捆绑 Node 运行时。

**核心约束（不可违背）**：dsh 前端必须由 dsh 服务端提供（它注入 `window.__DSH_BOOT__`），WebView 只能加载 `http://127.0.0.1:<port>`；桌面侧**零侵入**（只使用 dsh 公开接口：CLI `web --port 0`、公开下行流 `/api/events.mux`+`/api/events.host`、官方 `dsh plugin` 命令）。

## 2. 架构与关键机制

```
Tauri 2.11 壳 (Rust, apps/desktop/src-tauri/src/)
├─ dsh.rs        进程监督：spawn → 握手行解析("dsh web: http://127.0.0.1:<port>")
│                → 主窗口加载 → 退出清理(SIGTERM→3s→SIGKILL) → 孤儿回收(pidfile+ps -E 标记)
│                → 失败计数(≥3 可重置运行时) → 自动回滚(lastKnownGood)
├─ runtime.rs    受管运行时：appdata/runtime/{current 指针, versions/<ver>}
│                bootstrap(npm install → 验证 --version → 原子指针) / prune(保留 N) / reset_all
├─ update.rs     更新：npm view 检测(20s 超时) / 30min 结果缓存 / 手动+自动调度器
│                (60s tick, autoUpdate+intervalHours+lastCheck 防抖, 静默安装不自动重启)
│                / 版本列表+切换 / update-history.json
├─ listener.rs   WS 只读消费者(mux+host 下行流, fan-out 多消费者) → 事件映射 →
│                Toast 横幅(右上角, 6s 自隐, 点击聚焦) / macOS 系统通知(仅打包后) / Dock 徽标
├─ nodejs.rs     捆绑 Node 工具链解析(binaries/node-runtime/<os>-<arch>)
│                + enriched_path(前置 bundle bin 到 npm 生命周期脚本与子进程 PATH)
└─ lib.rs        窗口/托盘/菜单(编辑菜单=剪贴板!)/命令注册/单实例/信号处理
```

窗口：`splash`(壳 UI, 隐藏式生命周期, overlay 标题栏, 红叉=隐藏) / `main`(dsh GUI, 关闭走确认) / `toast`(横幅)。
UI：`apps/desktop/` Vite 多页(index.html 面板集 + toast.html)，事件协议 `dsh://state`、`dsh://ui-intent`、`dsh://update-line/done`、`dsh://plugin-log/done`、`dsh://toast`。

数据目录（macOS）：`~/Library/Application Support/com.dsh.desktop/`（settings.json / update-history.json / update-check-cache.json / logs/ / npm-cache/ / runtime/ / offline-plugins/ / dsh-child.pid）。
共享 `~/.dsh`（DSH_HOME 原样继承）；子进程 cwd=默认锚点 `~/dsh-desktop-workspace`（DSH_CWD 可覆盖）——工作区切换**完全交给 dsh GUI 内建机制**（会话按目录分组 + 原生目录选择器），桌面不做。

## 3. 已验收的能力清单（都有实测证据）

- M0-M1：spawn/握手/主窗口/GUI 200+`__DSH_BOOT__` 注入/SIGKILL 孤儿回收/错误页
- M2：首次 bootstrap（60s–2min，进度条）、二次启动秒级、settings 持久化
- M3：更新四路径全实测——自动检测+静默安装（假 registry 0.1.0-rc.7 演练）、手动切换激活（用户实操）、坏版本自动回滚（rolled-back 历史条目）、registry 不可达 lastCheck 持久化
- M4：单实例（二次启动退出+聚焦）、崩溃恢复（streak≥3 → 重置运行时按钮）、日志目录入口
- 补充需求：关闭确认+记住选择（settings.closeAction=ask|quit|background，注意当前被用户设为 **background**）；审批/提问 Toast（用户确认可见）；更新菜单面板；插件在线/离线（zip/目录，`add <本地路径> --offline`）
- M5-1：捆绑 Node v24.16.0（macos-aarch64, 194MB, gitignored）——**裸 PATH（/usr/bin:/bin）全流程安装验收通过**；安装进度改为不定进度条

## 4. 血泪教训（新会话必读，避免重蹈覆辙）

1. **`cargo check` 不产出二进制**——改完 Rust 要 `cargo build`，否则 tauri dev 跑旧二进制（曾导致两次假验收）。
2. **Tauri 同步命令在主线程内联执行**——任何阻塞调用（npm view、阻塞式文件选择器）必须 `async fn + spawn_blocking`，否则整个应用假死/死锁（`6c92b05`）。
3. **补丁脚本必须按文件实际文本匹配**——本会话多次因旧文本断言失败且"写盘在脚本末尾"导致静默丢补丁；改文件先 `read`/`grep` 实际内容，小步提交。
4. **git commit 消息含反引号/`$()` 会被 shell 展开**——用 `git commit -F - <<'MSG'`（heredoc 单引号定界）。
5. **macOS 剪贴板依赖「编辑」菜单预定义项**（copy/paste/cut/selectAll），没有则 Cmd+C/V/A 全废（`37f0195`）。
6. **macOS 通知**：tauri-plugin-notification 走已废弃 NSUserNotification 静默失败；UNUserNotificationCenter 在**无 .app bundle 的 dev 二进制下抛 NSException 直接崩溃**（bundleProxyForCurrentProcess is nil）——已用 Contents/MacOS 检测门控，仅打包后启用系统通知，dev 用 Toast。
7. **本机 `~/.npm` 缓存损坏**（root 文件 EPERM）——桌面所有 npm 操作必须 `--cache appdata/npm-cache`；仓库内 pnpm store/cargo home 重定向（`.npmrc`、`CARGO_HOME=.cargo-home`）。
8. **运行桌面应用需超 workspace 沙箱权限**：本会话所有启动/验收命令都带 `sandbox_permissions: danger-full-access`（写 ~/Library、spawn node、进程检查），新会话照旧。
9. **tauri dev 会跑 `cargo run --no-default-features`**：与裸 `cargo build`（默认 features）feature 集合不同，互相切换会触发全量重编（~1min），正常现象。
10. 窗口竞态三连（面板事件/Toast 载荷/意图）都靠**拉取模型**解决：Rust 状态存值 + 页面加载后 invoke 拉取 + 事件实时更新。

## 5. 开发运行

```bash
cd /Users/duxx/workspace/dsh-desktop
export CARGO_HOME=$PWD/.cargo-home          # 必需
pnpm install                                # store 在 .pnpm-store
pnpm desktop:dev                            # = pnpm --filter @dsh-desktop/shell tauri dev
# 注意：启动/验收需要 danger-full-access 沙箱权限
```

环境变量：`DSH_BIN`（覆盖 dsh 入口，dev 调试用）、`DSH_CWD`（覆盖子进程 cwd）、`DSH_NODE_DIR`（覆盖捆绑 Node 根）。
日志：应用 stdout 在 tauri dev 终端；运行日志落 `appdata/logs/dsh-<ts>.log`；菜单「打开日志目录…」。

## 6. 未完成 / 待决策

| 项 | 状态 | 备注 |
|----|------|------|
| **M5-2 壳自更新** | 未做 | tauri-plugin-updater + GitHub Releases latest.json，与 dsh 运行时更新正交，建议共用「检查更新」面板 |
| **M5-3 签名公证** | 未做 | 需要 Apple Developer ID（决策）；**打包后系统通知才会启用**（见教训 6）；未签名包分发需用户右键打开 |
| **M5-4 多平台验证** | 未做 | 全部实测在 macOS arm64；Windows(taskkill /T、WebView2)/Linux(webkit2gtk) 待实测；`RunEvent::Reopen` 为 macOS 专属 |
| **M5-5 打包产物** | 未做 | .dmg/NSIS/deb；bundle.resources 需接入 node-runtime（nodejs.rs 已预留 Contents/Resources 解析路径）；tauri icon 已生成 |
| **pnpm 捆绑** | 决策点 | dsh 插件功能依赖系统 pnpm（捆绑 Node 只带 corepack）；选项：检测+引导 vs 捆绑 pnpm 单文件 |
| 通知授权 UX | 可优化 | 打包后首次启动会弹系统通知授权；dev 恒用 Toast |
| 启动窗口阈值 1.2s | 可配置化 | 目前硬编码在 lib.rs setup 的 slow-start 线程 |

## 7. 文件地图

- `apps/desktop/src-tauri/`：Rust 壳（src/{dsh,lib,listener,nodejs,runtime,update}.rs、Cargo.toml、tauri.conf.json、capabilities/default.json、icons/（DeepSeek 图标）、binaries/node-runtime/（gitignored））
- `apps/desktop/`：Vite 壳 UI（index.html、toast.html、src/{main,toast}.ts、styles.css）
- `.intermediate/2026-08/13/001-tauri_wrapper/`：原始规划（05 有前置验证结果表）
- `README.md`：项目说明与里程碑状态

## 8. 快速自检命令

```bash
# 验证应用健康（需完整权限）：
grep -E "node toolchain|ready: http" /tmp/dsh-desktop-dev.log   # bundled + ready 即健康
# 版本目录 / 指针 / 历史：
ls ~/Library/Application\ Support/com.dsh.desktop/runtime/versions/
cat ~/Library/Application\ Support/com.dsh.desktop/runtime/current
cat ~/Library/Application\ Support/com.dsh.desktop/update-history.json
```
