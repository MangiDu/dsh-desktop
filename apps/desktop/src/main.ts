import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";


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
  "close-confirm": $("panel-close-confirm"),
  settings: $("panel-settings"),
};

function show(panel: keyof typeof panels) {
  for (const [name, el] of Object.entries(panels)) {
    el.classList.toggle("hidden", name !== panel);
  }
}

function appendLog(el: HTMLElement, line: string) {
  el.classList.remove("hidden");
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
  $("plugin-log").classList.remove("hidden");
  appendLog($("plugin-log"), `安装 ${pkg}…`);
  try {
    await invoke("dsh_plugin", { args: ["add", pkg] });
  } catch (err) {
    appendLog($("plugin-log"), `错误：${err}`);
  }
});

const offlineInstall = async (kind: "zip" | "dir") => {
  $("plugin-log").classList.remove("hidden");
  appendLog($("plugin-log"), kind === "zip" ? "请选择插件 zip 包…" : "请选择插件目录…");
  try {
    await invoke("dsh_plugin_offline", { kind });
  } catch (err) {
    appendLog($("plugin-log"), `错误：${err}`);
  }
};
$("btn-plugin-offline-zip").addEventListener("click", () => void offlineInstall("zip"));
$("btn-plugin-offline-dir").addEventListener("click", () => void offlineInstall("dir"));

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
type VersionInfo = { version: string; active: boolean; mtime: number };
type HistoryEntry = { at: number; from: string; to: string; result: string; error: string | null };

let versionsSeq = 0;
async function loadVersions() {
  const seq = ++versionsSeq;
  const list = $("version-list");
  list.textContent = "";
  try {
    const versions = await invoke<VersionInfo[]>("update_list_versions");
    if (seq !== versionsSeq) return; // a newer load superseded this one
    if (versions.length === 0) {
      const empty = document.createElement("p");
      empty.className = "hint";
      empty.textContent = "（无已安装版本）";
      list.appendChild(empty);
      return;
    }
    for (const v of versions) {
      if (seq !== versionsSeq) return; // superseded mid-render
      const row = document.createElement("div");
      row.className = "version-row";
      const tag = document.createElement("span");
      tag.className = "version-tag";
      tag.textContent = `v${v.version}`;
      row.appendChild(tag);
      if (v.active) {
        const mark = document.createElement("span");
        mark.className = "version-active";
        mark.textContent = "当前";
        row.appendChild(mark);
      } else {
        const btn = document.createElement("button");
        btn.textContent = "切换";
        btn.addEventListener("click", () => {
          void (async () => {
            $("update-status").textContent = `切换到 v${v.version}，重启中…`;
            try {
              await invoke("update_switch", { version: v.version });
            } catch (err) {
              $("update-status").textContent = `切换失败：${err}`;
            }
          })();
        });
        row.appendChild(btn);
      }
      list.appendChild(row);
    }
  } catch (err) {
    $("update-status").textContent = `版本列表加载失败：${err}`;
  }
}

async function loadHistory() {
  try {
    const entries = await invoke<HistoryEntry[]>("update_history_list");
    if (entries.length === 0) return;
    const logEl = $("update-history");
    logEl.classList.remove("hidden");
    logEl.classList.remove("placeholder-text");
    logEl.textContent = entries
      .map((e) => {
        const t = new Date(e.at * 1000).toLocaleString();
        const err = e.error ? `（${e.error}）` : "";
        return `${t}  ${e.from} → ${e.to}  ${e.result}${err}`;
      })
      .join("\n");
  } catch (err) {
    console.error("[shell] update_history_list failed:", err);
  }
}

async function runUpdateCheck(force = false) {
  setUpdateBusy(true, force ? "正在重新检查…" : "正在检查…");
  $("btn-update-apply").classList.add("hidden");
  $("btn-update-restart").classList.add("hidden");
  try {
    const info = await invoke<UpdateCheck>("update_check", force ? { force: true } : {});
    if (info.updatable) {
      $("update-status-text").textContent =
        `发现新版本 ${info.latest}（当前 ${info.current}）`;
      $("btn-update-apply").classList.remove("hidden");
    } else if (info.current === info.latest) {
      $("update-status-text").textContent = `已是最新版本：${info.current}`;
    } else {
      $("update-status-text").textContent =
        `当前 ${info.current}（自定义 DSH_BIN，不适用更新），最新 ${info.latest}`;
    }
  } catch (err) {
    $("update-status-text").textContent = `检查失败：${err}`;
  } finally {
    setUpdateBusy(false);
  }
}

function setUpdateBusy(busy: boolean, text?: string) {
  const spinner = $("update-spinner");
  const btn = $("btn-update-recheck") as HTMLButtonElement;
  spinner.classList.toggle("hidden", !busy);
  btn.disabled = busy;
  btn.textContent = busy ? "检查中…" : "重新检查";
  if (text !== undefined) $("update-status-text").textContent = text;
}

$("btn-update-recheck").addEventListener("click", () => {
  void runUpdateCheck(true);
});

$("btn-update-apply").addEventListener("click", async () => {
  $("update-status").textContent = "正在安装，请稍候…";
  $("btn-update-apply").classList.add("hidden");
  $("update-log").classList.remove("hidden");
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

listen<string>("dsh://update-line", (event) => {
  appendLog($("update-log"), event.payload);
});

listen<UpdateDone>("dsh://update-done", (event) => {
  const done = event.payload;
  if (done.ok) {
    $("update-status").textContent = `v${done.version} 已就绪`;
    appendLog($("update-log"), "✓ 安装完成");
    $("btn-update-restart").classList.remove("hidden");
    void loadVersions();
    void loadHistory();
  } else {
    $("update-status").textContent = "更新未完成";
    if (done.error) appendLog($("update-log"), `✗ ${done.error}`);
    $("btn-update-apply").classList.remove("hidden");
  }
});

// Close-confirm panel (remember choice checkbox).
const sendCloseChoice = async (quit: boolean) => {
  const remember = ($("close-remember") as HTMLInputElement).checked;
  try {
    await invoke("close_choice", { quit, remember });
  } catch (err) {
    console.error("[shell] close_choice failed:", err);
  }
};
$("btn-close-quit").addEventListener("click", () => void sendCloseChoice(true));
$("btn-close-background").addEventListener("click", () => void sendCloseChoice(false));

// Settings panel.
type Settings = {
  channel: string;
  autoUpdate: boolean;
  intervalHours: number;
  registry: string;
  lastCheck: number | null;
  lastProjectDir: string | null;
  closeAction: string;
  keepVersions: number;
};

async function loadSettings() {
  try {
    const s = await invoke<Settings>("settings_get");
    ($("set-close-action") as HTMLSelectElement).value = s.closeAction;
    ($("set-channel") as HTMLSelectElement).value = s.channel;
    ($("set-auto-update") as HTMLInputElement).checked = s.autoUpdate;
    ($("set-interval") as HTMLInputElement).value = String(s.intervalHours);
    ($("set-registry") as HTMLInputElement).value = s.registry;
    ($("set-keep-versions") as HTMLInputElement).value = String(s.keepVersions ?? 1);
    $("settings-status").textContent = "";
  } catch (err) {
    $("settings-status").textContent = `加载失败：${err}`;
  }
}

$("btn-settings-save").addEventListener("click", async () => {
  const settings: Settings = {
    channel: ($("set-channel") as HTMLSelectElement).value,
    autoUpdate: ($("set-auto-update") as HTMLInputElement).checked,
    intervalHours: Number(($("set-interval") as HTMLInputElement).value) || 12,
    registry: ($("set-registry") as HTMLInputElement).value.trim() || "https://registry.npmjs.org",
    lastCheck: null,
    lastProjectDir: null,
    closeAction: ($("set-close-action") as HTMLSelectElement).value,
    keepVersions: Math.min(10, Math.max(1, Number(($("set-keep-versions") as HTMLInputElement).value) || 1)),
  };
  try {
    await invoke("settings_set", { settings });
    $("settings-status").textContent = "已保存 ✓";
  } catch (err) {
    $("settings-status").textContent = `保存失败：${err}`;
  }
});

function openPanel(intent: string | null) {
  if (intent === "plugin") show("plugin");
  if (intent === "update") {
    show("update");
    void runUpdateCheck();
    void loadVersions();
    void loadHistory();
  }
  if (intent === "close-confirm") show("close-confirm");
  if (intent === "settings") {
    show("settings");
    void loadSettings();
  }
}

// Live switch while the shell window is already open (menu clicks with a
// loaded window; the host pushes the panel through this event).
listen<string>("dsh://ui-intent", (event) => {
  openPanel(event.payload);
});

// Re-opened for a specific panel? The host stores the intent once.
invoke<string | null>("ui_intent")
  .then((intent) => openPanel(intent))
  .catch((err) => console.error("[shell] ui_intent failed:", err));
