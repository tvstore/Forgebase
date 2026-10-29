import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

// The desk is served by `forgebase serve` under /desk/ (FORGEBASE_DESK_DIR=desk/dist).
// `npm run dev` proxies the API to a locally running `forgebase serve`.
export default defineConfig({
  base: '/desk/',
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    assetsDir: 'assets',
    // Every script and style is a separate file: the CSP forbids inline code.
    assetsInlineLimit: 0,
    cssCodeSplit: false,
  },
  server: {
    proxy: { '/api': 'http://localhost:8000' },
  },
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
});
