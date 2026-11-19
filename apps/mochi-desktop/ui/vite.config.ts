import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Tauri loads the dev server at a fixed port (tauri.conf.json `devUrl`) and
// the built files from `dist` (`frontendDist`). Nothing is fetched from the
// network at run time: the CSP allows only the app's own origin and IPC.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5173, strictPort: true, host: "localhost" },
  build: {
    outDir: "dist",
    target: ["es2022", "chrome110", "safari16"],
    sourcemap: false,
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test/setup.ts"],
  },
});
