import adapter from '@sveltejs/adapter-static';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	kit: {
		adapter: adapter({
			pages: 'dist',
			assets: 'dist',
			// Pure SPA: one prerendered shell for every route, hydration after.
			fallback: 'index.html',
			strict: false
		})
	}
};

export default config;
