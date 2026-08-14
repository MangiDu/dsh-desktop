# 003 · macOS「需要二次点击」问题专案记录

> 日期：2026-08-14 · 状态：**已修复并验收（稳定一次触达）**
> 相关提交：`503a4a9` `6d7f657` `7713a86` `16cfa03` `6257f52` `0ffc2a2`

## 1. 症状

desktop 内偶发/稳定复现：侧栏工作区/会话条目**来回快速点击**时，部分点击
「有视觉反馈但动作没执行」，需再点一次；web 网页端（Chrome）一次触达。

用户给出的关键判据（决定排查方向）：

- 复现动作：dsh 界面左侧工作区/会话列表之间切换点击；
- 表现：**有视觉反馈但动作没执行**（说明事件到达了页面，不是原生吞点击）；
- 窗口焦点日志（`notify.log`）证明复现期间主窗口 key 状态**无变化**。

## 2. 排查过程（探针方法论）

给主窗口注入只读捕获探针（`initialization_script`，git 历史 `16cfa03`），
逐轮加码直到唯一化机制：

| 轮次 | 探针内容 | 结论 |
|------|---------|------|
| 1 | mousedown/mouseup/click 目标类名 + 坐标 + detail | 失败手势：down/up 同目标、**无 click**、up 的 `detail=0`——WebKit 主动抑制了 click |
| 2 | + pointer 系列 + defaultPrevented + dblclick | 无 pointercancel、无 preventDefault |
| 3 | + **节点同一性**（down/up 是否同一 DOM 节点、down 节点是否仍连接）+ **按下期间 scroll 计数** | 失败手势：**`same=0`、`connected=1`、`scrolled=1`** → 机制锁定 |

途中被排除（并保留为加固）的假设：

- 窗口非 key 首次点击被吞 → `acceptsFirstMouse` 全局 swizzle（`503a4a9`）
  + 菜单结束跟踪/应用激活后归还 key（`6d7f657`）；
- Force Touch 查词手势吞点击 → WKWebView `pressureConfiguration = nil`
  （`7713a86`）；
- 重渲染换节点（React 重渲染导致 down/up 不同节点）→ `connected=1` 排除。

## 3. 最终机制

**dsh 侧栏在 mousedown 瞬间会发生一次滚动**（条目聚焦/选中触发），光标下的
DOM 节点被换掉：mousedown 落在节点 A，滚动后 mouseup 落在节点 B（类名相同，
故早期探针看不出差异），WebKit 按规则丢弃 click。快速来回点击时每次都踩在
滚动间隙里，因此稳定复现；Chrome 无此滚动时序（或时序不同），故 web 端正常。

## 4. 修复

壳层注入**点击补偿器**（`CLICK_COMPENSATOR_SCRIPT`，仅主窗口）：

- 左键按下后 90ms 内浏览器未派发 click；
- 光标位移 < 6px（排除真实拖拽）；
- 按下节点仍在文档中（排除节点被移除）；
- → 在原节点补发一次 `click`（bubbles/cancelable/detail=1）。

门控保证双击、右键/中键、拖拽、正常点击路径完全不受影响。
验收：用户实测「稳定一次触达」。

## 5. 保留的配套加固

- `acceptsFirstMouse` swizzle + 菜单/激活后主窗口 key 归还（`lib.rs`）；
- WKWebView `pressureConfiguration=nil`（`dsh.rs` on_ready）；
- `devtools` feature + `.devtools(true)`（release 可被 Safari 检查）；
- `capabilities/main.json`：主窗口 `notification:default`（回环 http 源限定）。

## 6. 教训

- 「类名相同」不等于「同一节点」——探针必须记录**节点同一性**；
- WKWebView 的 click 抑制有明确指纹：up 的 `detail=0` 且无 click；
- 逐层假设 + 一次只改一个变量，才能收敛到唯一机制；
- wry 的 `set_inspectable` 只在 debug 或 tauri `devtools` feature 下编译，
  release 排查必须显式开启该 feature。
