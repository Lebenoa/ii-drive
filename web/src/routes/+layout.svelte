<script lang="ts">
	import '../app.css';
	import { page } from '$app/state';
	import Navbar from '$lib/components/TopBar.svelte';

	let { children } = $props();
</script>

{#if page.url.pathname !== '/login'}
	<Navbar />
{/if}

{@render children()}
