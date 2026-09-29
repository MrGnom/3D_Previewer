import { defineConfig } from "vite";

// Relative base: the same bundle is served by Tauri (at the origin root) and by the
// Explorer preview handler (under /viewer/ on a WebView2-intercepted host).
export default defineConfig({
    base: "./",
    clearScreen: false,
    // fs.allow lets the dev server read ../samples via /@fs/ URLs.
    server: { port: 5173, strictPort: true, fs: { allow: [".."] } },
    build: {
        target: "es2022",
        outDir: "dist",
        emptyOutDir: true,
        chunkSizeWarningLimit: 8000,
        // One script instead of ~200 lazy chunks: the preview pane serves every request
        // through WebView2 interception, so fewer round-trips means a faster first frame.
        rolldownOptions: { output: { inlineDynamicImports: true } },
    },
});
