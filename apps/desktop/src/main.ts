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
};

function show(panel: keyof typeof panels) {
  for (const [name, el] of Object.entries(panels)) {
    el.classList.toggle("hidden", name !== panel);
  }
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
  $("plugin-log").textContent = `安装 ${pkg}…\n`;
  try {
    await invoke("dsh_plugin", { args: ["add", pkg] });
  } catch (err) {
    $("plugin-log").textContent += `错误：${err}\n`;
  }
});

$("btn-plugin-close").addEventListener("click", async () => {
  await getCurrentWindow().close();
});

listen<string>("dsh://plugin-log", (event) => {
  $("plugin-log").textContent += event.payload + "\n";
});

listen<number>("dsh://plugin-done", (event) => {
  $("plugin-log").textContent +=
    event.payload === 0 ? "\n✓ 完成（重启 dsh 后生效）" : `\n✗ 失败（退出码 ${event.payload}）`;
});

// Re-opened for a specific panel? The host stores the intent once.
invoke<string | null>("ui_intent")
  .then((intent) => {
    if (intent === "plugin") show("plugin");
  })
  .catch((err) => console.error("[shell] ui_intent failed:", err));
