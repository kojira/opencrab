import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'path';

// The core REST API the dev server forwards /api to. OPENCRAB_API_URL follows the core's
// [gateway.rest] port (e.g. http://localhost:18080); default http://localhost:8080.
const apiUrl = process.env.OPENCRAB_API_URL || 'http://localhost:8080';

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    proxy: {
      '/api/web-conversations': {
        target: 'http://127.0.0.1:8787',
        changeOrigin: true,
      },
      '/api': {
        target: apiUrl,
        changeOrigin: true,
      },
    },
  },
});
