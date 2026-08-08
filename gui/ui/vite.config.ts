import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// The window loads this build from disk through Tauri's asset protocol, so the
// output must be relative and self-contained: no CDN, no absolute origin. The
// window's CSP refuses anything else.
/**
 * Strips the `crossorigin` attribute Vite puts on its emitted tags.
 *
 * The window serves this page over Tauri's asset protocol, where a
 * cross-origin-marked module script fails its CORS check and is never
 * executed — silently, leaving a blank window and an empty process log.
 * The assets are same-origin by construction, so the attribute is noise.
 */
function sameOriginAssets(): Plugin {
  return {
    name: "youta-same-origin-assets",
    enforce: "post",
    transformIndexHtml(html) {
      return html.replace(/\s+crossorigin(=["'][^"']*["'])?/g, "");
    },
  };
}

export default defineConfig({
  plugins: [react(), tailwindcss(), sameOriginAssets()],
  base: "./",
  build: {
    outDir: "../frontend",
    emptyOutDir: true,
    target: "safari16",
    // One file each keeps the CSP simple and the asset protocol predictable.
    rollupOptions: {
      output: {
        entryFileNames: "app.js",
        chunkFileNames: "app-[hash].js",
        assetFileNames: "app.[ext]",
      },
    },
  },
  clearScreen: false,
});
