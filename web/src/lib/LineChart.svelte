<script lang="ts">
	import type { Sample } from './api.ts';
	import { areaPath, linePath, scalePoints } from './chart.ts';

	let {
		samples,
		key,
		minutes = 30,
		now
	}: {
		samples: Sample[];
		key: 'cpu' | 'memory' | 'swap' | 'disk';
		minutes?: number;
		now: number;
	} = $props();

	const W = 300;
	const H = 80;
	const points = $derived(scalePoints(samples, key, now - minutes * 60, now, W, H));
</script>

<svg
	viewBox="0 0 {W} {H}"
	preserveAspectRatio="none"
	class="h-20 w-full"
	role="img"
	aria-label="{key} history"
>
	{#each [0.25, 0.5, 0.75] as g (g)}
		<line x1="0" x2={W} y1={H * g} y2={H * g} class="stroke-surface-300-700" stroke-width="0.5" />
	{/each}
	{#if points.length > 1}
		<path d={areaPath(points, H)} class="fill-primary-500/20" />
		<path
			d={linePath(points)}
			class="fill-none stroke-primary-500"
			stroke-width="1.5"
			vector-effect="non-scaling-stroke"
		/>
	{/if}
</svg>
