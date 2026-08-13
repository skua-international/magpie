import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // gateway mounts this under /ui (see services/gateway/src/main.rs), so
  // every emitted asset URL has to be prefixed to match -- with the
  // default "/" the bundle would request /assets/*, which falls through
  // to the Connect catch-all and comes back as an RPC error, not JS.
  base: "/ui/",
  build: { outDir: "dist", emptyOutDir: true },
  server: {
    // `npm run dev` serves the SPA itself but has no backend; point RPC
    // and auth calls at a real cluster with
    // MAGPIE_DEV_API=https://api.<baseDomain> npm run dev.
    // That origin must be in identity.allowedRedirectOrigins for login
    // to complete, unless it's loopback (which is always allowed).
    proxy: process.env.MAGPIE_DEV_API
      ? {
          "/auth": { target: process.env.MAGPIE_DEV_API, changeOrigin: true, secure: false },
          "/registry.v1.": { target: process.env.MAGPIE_DEV_API, changeOrigin: true, secure: false },
          "/controller.v1.": { target: process.env.MAGPIE_DEV_API, changeOrigin: true, secure: false },
        }
      : undefined,
  },
});
