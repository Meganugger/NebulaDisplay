import { defineConfig } from "vite";
import { resolve } from "node:path";

export default defineConfig({
  build: {
    target: "es2022",
    rollupOptions: {
      input: {
        viewer: resolve(__dirname, "index.html"),
        panel: resolve(__dirname, "panel.html"),
      },
    },
  },
});
