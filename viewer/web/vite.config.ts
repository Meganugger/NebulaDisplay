import { defineConfig } from "vite";
import { resolve } from "node:path";

// Multi-page app: `/` is the host control panel, `/view/` is the viewer.
export default defineConfig({
  build: {
    rollupOptions: {
      input: {
        panel: resolve(__dirname, "index.html"),
        viewer: resolve(__dirname, "view/index.html"),
      },
    },
  },
  server: {
    proxy: {
      "/api": "http://localhost:38470",
      "/ws": { target: "ws://localhost:38470", ws: true },
    },
  },
});
