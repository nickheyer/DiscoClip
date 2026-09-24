import tailwindcss from '@tailwindcss/vite';
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

/// Where the built app goes. `crates/app/build.rs` sets it to a directory it then embeds.
const out = process.env.DISCOCLIP_UI_OUT ?? 'build';

/// The backend the dev server forwards `/api` to.
const api = process.env.DISCOCLIP_API ?? 'http://127.0.0.1:8080';

export default defineConfig({
	plugins: [
		tailwindcss(),
		sveltekit({
			compilerOptions: {
				// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},
			adapter: adapter({ pages: out, assets: out, fallback: 'index.html' })
		})
	],
	build: {
		// Fonts stay files: the server's CSP allows `font-src 'self'` only, so an inlined
		// `data:` font would be blocked.
		assetsInlineLimit: (file) => (/\.(woff2?|ttf|otf)$/i.test(file) ? false : undefined)
	},
	server: {
		proxy: {
			// The Host header stays the dev server's, so the backend's Origin check passes.
			// Event streams pass through without buffering.
			'/api': { target: api, changeOrigin: false }
		}
	}
});
