import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// Cargo's build script points this at its own output directory so the built app is
// embedded straight from there; a plain `npm run build` writes to `build/`.
const out = process.env.DISCOCLIP_UI_OUT ?? 'build';

export default defineConfig({
	plugins: [
		sveltekit({
			compilerOptions: {
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},
			adapter: adapter({
				pages: out,
				assets: out,
				fallback: 'index.html',
				precompress: false,
				strict: true
			})
		})
	],
	server: {
		proxy: {
			'/api': {
				target: process.env.DISCOCLIP_API ?? 'http://127.0.0.1:8080',
				changeOrigin: false
			}
		}
	}
});
