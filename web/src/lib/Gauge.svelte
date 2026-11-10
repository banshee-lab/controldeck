<script lang="ts">
	import { Progress } from '@skeletonlabs/skeleton-svelte';

	let { value, label }: { value: number; label: string } = $props();

	const clamped = $derived(Math.min(100, Math.max(0, value)));
	const range = $derived(
		clamped >= 90 ? 'stroke-error-500' : clamped >= 70 ? 'stroke-warning-500' : 'stroke-success-500'
	);
</script>

<div class="flex flex-col items-center gap-1">
	<Progress value={clamped} class="w-fit">
		<div class="relative">
			<div class="absolute inset-0 flex items-center justify-center text-lg font-semibold">
				{Math.round(clamped)}%
			</div>
			<Progress.Circle class="[--size:--spacing(24)]">
				<Progress.CircleTrack class="stroke-surface-200-800" />
				<Progress.CircleRange class={range} />
			</Progress.Circle>
		</div>
	</Progress>
	<span class="text-sm opacity-70">{label}</span>
</div>
