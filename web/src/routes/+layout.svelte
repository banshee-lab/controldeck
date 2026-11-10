<script lang="ts">
	import './layout.css';
	import favicon from '#lib/assets/favicon.svg';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
</svelte:head>

<div class="mx-auto flex min-h-screen max-w-7xl flex-col gap-4 p-4">
	<header class="flex items-center justify-between">
		<h1 class="h3">ControlDeck</h1>
		<span class="badge preset-tonal-primary">live</span>
	</header>
	<main class="flex flex-col gap-4">
		{@render children()}
	</main>
</div>
