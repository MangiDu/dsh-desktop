import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

type ToastPayload = { title: string; body: string };

listen<ToastPayload>("dsh://toast", (event) => {
  const { title, body } = event.payload;
  document.getElementById("toast-title")!.textContent = title;
  document.getElementById("toast-body")!.textContent = body;
});

// The emit can race ahead of a freshly created window's page load:
// pull the latest payload once, then keep live updates via the event.
invoke<[string, string] | null>("toast_current")
  .then((payload) => {
    if (payload) {
      document.getElementById("toast-title")!.textContent = payload[0];
      document.getElementById("toast-body")!.textContent = payload[1];
    }
  })
  .catch(() => {});

// Clicking the banner focuses the main window and dismisses it.
document.body.addEventListener("click", () => {
  void invoke("toast_clicked");
});
