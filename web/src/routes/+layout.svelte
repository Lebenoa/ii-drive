<script lang="ts">
	import '../app.css';
	import { page } from '$app/state';
	import { onNavigate } from '$app/navigation';
	import Navbar from '$lib/components/TopBar.svelte';
	import { reducedMotion } from '$lib/motion';
	import { initI18n, i18nReady } from '$lib/i18n.svelte';

	let { children } = $props();

	// Download the saved (or English) dictionary before first paint so the
	// app never flashes raw message keys.
	$effect(() => {
		void initI18n();
	});

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

{#if i18nReady()}
	<!-- No chrome on the pages that exist before an account does: the bar's
	     links all lead somewhere unreachable there. -->
	{#if page.url.pathname !== '/login' && page.url.pathname !== '/setup'}
		<Navbar />
	{/if}

	{@render children()}
{/if}
