import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * Shell UI contract (M1):
 * - Rust emits `dsh://state` events: { phase, url?, code?, logTail? }
 * - Commands: `dsh_status`, `dsh_restart`, `app_quit`
 */

type DshState =
  | { phase: "starting" }
  | { phase: "bootstrapping"; line: string }
  | { phase: "ready"; url: string; version?: string }
  | { phase: "start-failed"; reason: string; logTail: string }
  | { phase: "exited"; code: number | null; logTail: string };

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
};

const panels = {
  splash: $("panel-splash"),
  error: $("panel-error"),
  failed: $("panel-failed"),
  plugin: $("panel-plugin"),
  update: $("panel-update"),
};

function show(panel: keyof typeof panels) {
  for (const [name, el] of Object.entries(panels)) {
    el.classList.toggle("hidden", name !== panel);
  }
}

function appendLog(el: HTMLElement, line: string) {
  if (el.classList.contains("placeholder-text")) {
    el.classList.remove("placeholder-text");
    el.textContent = "";
  }
  el.textContent += line + "\n";
  el.scrollTop = el.scrollHeight;
}

function logTailOf(tail: string): string {
  const lines = tail.split("\n").filter((l) => l.trim().length > 0);
  return lines.slice(-12).join("\n") || "(无输出)";
}

async function onState(state: DshState) {
  console.log("[shell] state:", state.phase);
  switch (state.phase) {
    case "starting":
      $("splash-hint").textContent = "正在启动 dsh…";
      show("splash");
      break;
    case "bootstrapping":
      $("splash-hint").textContent = state.line;
      break;
    case "ready":
      $("splash-hint").textContent = "dsh 已就绪";
      // Main window is created by Rust; the splash window closes itself.
      break;
    case "start-failed":
      $("error-hint").textContent = state.reason;
      $("error-log").textContent = logTailOf(state.logTail);
      show("error");
      break;
    case "exited":
      $("failed-hint").textContent = `dsh 进程已退出（code=${state.code ?? "?"}）`;
      $("failed-log").textContent = logTailOf(state.logTail);
      show("failed");
      break;
  }
}

$("btn-retry").addEventListener("click", async () => {
  show("splash");
  await invoke("dsh_restart");
});

$("btn-restart").addEventListener("click", async () => {
  show("splash");
  await invoke("dsh_restart");
});

for (const id of ["btn-quit", "btn-quit2"]) {
  $(id).addEventListener("click", async () => {
    await invoke("app_quit");
  });
}

listen<DshState>("dsh://state", (event) => {
  void onState(event.payload);
});

// Ask for the current state once in case we were (re)loaded late.
invoke<DshState>("dsh_status").then((state) => void onState(state)).catch((err) => {
  console.error("[shell] dsh_status failed:", err);
});

// Plugin panel (supplementary requirement #4).
$("btn-plugin-add").addEventListener("click", async () => {
  const input = $("plugin-pkg") as HTMLInputElement;
  const pkg = input.value.trim();
  if (!pkg) return;
  appendLog($("plugin-log"), `安装 ${pkg}…`);
  try {
    await invoke("dsh_plugin", { args: ["add", pkg] });
  } catch (err) {
    appendLog($("plugin-log"), `错误：${err}`);
  }
});

$("btn-plugin-close").addEventListener("click", async () => {
  await getCurrentWindow().close();
});

listen<string>("dsh://plugin-log", (event) => {
  appendLog($("plugin-log"), event.payload);
});

listen<number>("dsh://plugin-done", (event) => {
  appendLog(
    $("plugin-log"),
    event.payload === 0 ? "✓ 完成（重启 dsh 后生效）" : `✗ 失败（退出码 ${event.payload}）`,
  );
});

// Update panel (supplementary requirement #3).
type UpdateCheck = { current: string; latest: string; updatable: boolean };
type UpdateDone = { ok: boolean; version: string; error: string | null };

async function runUpdateCheck() {
  $("update-status").textContent = "正在检查…";
  $("btn-update-apply").classList.add("hidden");
  $("btn-update-restart").classList.add("hidden");
  try {
    const info = await invoke<UpdateCheck>("update_check");
    if (info.updatable) {
      $("update-status").textContent =
        `发现新版本 ${info.latest}（当前 ${info.current}）`;
      $("btn-update-apply").classList.remove("hidden");
    } else if (info.current === info.latest) {
      $("update-status").textContent = `已是最新版本：${info.current}`;
    } else {
      $("update-status").textContent =
        `当前 ${info.current}（自定义 DSH_BIN，不适用更新），最新 ${info.latest}`;
    }
  } catch (err) {
    $("update-status").textContent = `检查失败：${err}`;
  }
}

$("btn-update-apply").addEventListener("click", async () => {
  $("update-status").textContent = "正在安装，请稍候…";
  $("btn-update-apply").classList.add("hidden");
  try {
    await invoke("update_apply");
  } catch (err) {
    appendLog($("update-log"), `错误：${err}`);
  }
});

$("btn-update-restart").addEventListener("click", async () => {
  show("splash");
  await invoke("dsh_restart");
});

$("btn-update-close").addEventListener("click", async () => {
  await getCurrentWindow().close();
});

listen<string>("dsh://update-line", (event) => {
  appendLog($("update-log"), event.payload);
});

listen<UpdateDone>("dsh://update-done", (event) => {
  const done = event.payload;
  if (done.ok) {
    $("update-status").textContent = `v${done.version} 已就绪`;
    appendLog($("update-log"), "✓ 安装完成");
    $("btn-update-restart").classList.remove("hidden");
  } else {
    $("update-status").textContent = "更新未完成";
    if (done.error) appendLog($("update-log"), `✗ ${done.error}`);
    $("btn-update-apply").classList.remove("hidden");
  }
});

// Re-opened for a specific panel? The host stores the intent once.
invoke<string | null>("ui_intent")
  .then((intent) => {
    if (intent === "plugin") show("plugin");
    if (intent === "update") {
      show("update");
      void runUpdateCheck();
    }
  })
  .catch((err) => console.error("[shell] ui_intent failed:", err));
