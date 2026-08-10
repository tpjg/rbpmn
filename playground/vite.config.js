import { defineConfig } from 'vite';

// The playground stays a static page; instance inspection reaches a running
// rbpmn-server through this dev proxy, so the server never needs CORS
// headers (its API stays browser-hostile by design — see docs/http-security.md).
const proxy = {
  '/rbpmn': {
    target: process.env.RBPMN_SERVER_URL ?? 'http://127.0.0.1:7420',
    changeOrigin: true,
    rewrite: (path) => path.replace(/^\/rbpmn/, ''),
  },
};

export default defineConfig({
  server: { proxy },
  preview: { proxy },
});
