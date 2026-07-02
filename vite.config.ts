import { defineConfig } from 'vite';
import { fileURLToPath, URL } from 'node:url';

const r = (p: string): string => fileURLToPath(new URL(p, import.meta.url));

// Multi-page vanilla-TS app: one bundle per window (popup, ask).
// The Tauri shell loads these by filename (popup.html / ask.html).
export default defineConfig({
  root: r('./ui'),
  base: './',
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: r('./ui/dist'),
    emptyOutDir: true,
    target: 'es2020',
    rollupOptions: {
      input: {
        popup: r('./ui/popup.html'),
        ask: r('./ui/ask.html'),
      },
    },
  },
});
