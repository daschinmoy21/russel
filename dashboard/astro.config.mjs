import { defineConfig } from "astro/config";

export default defineConfig({
	output: "static",
	server: {
		port: 4321,
		// Loopback only by default (#188). Use `bun run dev:lan` to bind 0.0.0.0
		// — never on a shared LAN without RUSSEL_API_TOKEN on the control plane.
		host: "127.0.0.1",
	},
	vite: {
		server: {
			proxy: {
				"/api": {
					target: process.env.RUSSEL_API_PROXY || "http://127.0.0.1:7878",
					changeOrigin: true,
					rewrite: (p) => p.replace(/^\/api/, ""),
				},
			},
		},
	},
});
