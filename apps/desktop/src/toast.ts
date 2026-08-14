import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

type ToastPayload = { title: string; body: string };

listen<ToastPayload>("dsh://toast", (event) => {
  const { title, body } = event.payload;
  document.getElementById("toast-title")!.textContent = title;
  document.getElementById("toast-body")!.textContent = body;
});

// Clicking the banner focuses the main window and dismisses it.
document.body.addEventListener("click", () => {
  void invoke("toast_clicked");
});
