import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Built into the onto binary: `npm run build` writes crates/onto-grpc/ui-dist,
// which `onto serve` serves at /ui. `npm run dev` proxies the API to a local server.
const target = process.env.ONTO_URL ?? "http://127.0.0.1:50051";

export default defineConfig({
  base: "/ui/",
  plugins: [react(), tailwindcss()],
  build: {
    outDir: "../crates/onto-grpc/ui-dist",
    emptyOutDir: true,
    assetsDir: "assets",
    sourcemap: false,
    chunkSizeWarningLimit: 900,
  },
  server: {
    port: 5173,
    proxy: {
      "/v1": { target, changeOrigin: true },
      "/ui/config.json": { target, changeOrigin: true },
      "/healthz": { target, changeOrigin: true },
    },
  },
});
