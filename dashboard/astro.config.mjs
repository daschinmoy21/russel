import { defineConfig } from "astro/config";

export default defineConfig({
	output: "static",
	server: {
		port: 4321,
		host: true,
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
