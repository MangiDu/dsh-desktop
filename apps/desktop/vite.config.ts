import { defineConfig } from "vite";

// The shell UI is a single page (splash / error / status). Vite serves it in
// dev at the fixed port Tauri's devUrl points at (tauri.conf.json).
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
  },
});
