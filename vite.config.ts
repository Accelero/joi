import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Vite + Vitest config. Tests run in `node` (pure DSP/IPC logic only): jsdom cannot run
// AudioWorklet / getUserMedia / getDisplayMedia / WebGL, so the media glue is verified by the
// M0 spike and per-milestone manual demos instead (PLAN §5, M-4).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  test: {
    environment: "node",
    globals: true,
    include: ["src/**/*.test.ts"],
  },
});
