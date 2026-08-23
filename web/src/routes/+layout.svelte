<script lang="ts">
	import '../app.css';
	import { page } from '$app/state';
	import { onNavigate } from '$app/navigation';
	import Navbar from '$lib/components/TopBar.svelte';
	import { reducedMotion } from '$lib/motion';

	let { children } = $props();

	// Cross-route crossfade via the View Transition API. Browsers without it
	// (Firefox, Safari < 18.2) navigate instantly — no polyfill, no layout
	// shift. Keyframes live in app.css under ::view-transition-*.
	onNavigate((navigation) => {
		if (!document.startViewTransition || reducedMotion()) return;
		return new Promise((resolve) => {
			document.startViewTransition(async () => {
				resolve();
				await navigation.complete;
			});
		});
	});
</script>

{#if page.url.pathname !== '/login'}
	<Navbar />
{/if}

{@render children()}
