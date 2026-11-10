import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The fixed port is part of the Tauri config (`build.devUrl`). Keep both sides
// in sync if the port changes.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "es2021",
    sourcemap: false,
  },
});
