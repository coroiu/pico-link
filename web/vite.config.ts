/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// https://vite.dev/config/
export default defineConfig({
  // Relative base so the static build works from any path (a GitHub Pages
  // project subpath, a plain file host, etc) -- design requirement
  // (pico-link-jyhk.10): "static build output must be host-agnostic".
  base: "./",
  plugins: [react(), tailwindcss()],
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test/setup.ts"],
  },
});
