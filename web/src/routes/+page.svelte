<script lang="ts">
	import { onMount } from 'svelte';
	import { fetchCurrent, fetchHistory, type Current, type Sample } from '#lib/api.ts';
	import { formatBytes, formatPercent, formatTime, formatUptime } from '#lib/format.ts';
	import Gauge from '#lib/Gauge.svelte';
	import LineChart from '#lib/LineChart.svelte';

	const MINUTES = 30;
	let current = $state<Current | null>(null);
	let history = $state<Sample[]>([]);
	let error = $state<string | null>(null);

	async function refresh() {
		try {
			[current, history] = await Promise.all([fetchCurrent(), fetchHistory(MINUTES)]);
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		}
	}

	onMount(() => {
		refresh();
		const id = setInterval(refresh, 5000);
		return () => clearInterval(id);
	});

	const cards = $derived(
		current
			? ([
					{ key: 'cpu', title: 'CPU', value: current.sample.cpu },
					{ key: 'memory', title: 'Memory', value: current.sample.memory },
					{ key: 'swap', title: 'Virtual memory', value: current.sample.swap },
					{ key: 'disk', title: 'Disk', value: current.sample.disk }
				] as const)
			: []
	);

	const rows = $derived.by(() => {
		if (!current) return [];
		const s = current.system;
		const m = s.memory;
		const d = s.disk;
		return [
			['Hostname', s.hostname],
			['Operating system', s.os],
			['System time', formatTime(s.time)],
			['Uptime', formatUptime(s.uptime_secs)],
			['Kernel', s.kernel],
			['CPU', `${s.cpu_model} (${s.cpu_cores} cores)`],
			['Processes', `${s.processes_total} total, ${s.processes_running} running`],
			['Load average (1/5/15 min)', s.load_avg.map((v) => v.toFixed(2)).join(' / ')],
			[
				'Real memory',
				`${formatBytes(m.used)} used, ${formatBytes(m.cached)} cached, ${formatBytes(m.total)} total`
			],
			['Virtual memory', `${formatBytes(m.swap_used)} used, ${formatBytes(m.swap_total)} total`],
			[
				'Local disk',
				`${formatBytes(d.used)} used, ${formatBytes(d.free)} free, ${formatBytes(d.total)} total`
			]
		];
	});
</script>

{#if error}
	<div class="card preset-filled-error-500 p-4" role="alert">Backend unreachable: {error}</div>
{/if}

{#if current}
	<section class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
		{#each cards as c (c.key)}
			<div class="flex flex-col gap-2 card preset-filled-surface-100-900 p-4">
				<h2 class="h6">{c.title}</h2>
				<Gauge value={c.value} label={formatPercent(c.value)} />
				<LineChart samples={history} key={c.key} minutes={MINUTES} now={current.system.time} />
				<span class="text-xs opacity-60">last {MINUTES} min</span>
			</div>
		{/each}
	</section>

	<section class="card preset-filled-surface-100-900 p-4">
		<h2 class="mb-2 h5">System status</h2>
		<div class="table-wrap">
			<table class="table">
				<tbody>
					{#each rows as [name, value] (name)}
						<tr>
							<th class="w-1/3 text-left font-medium">{name}</th>
							<td>{value}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</section>
{:else if !error}
	<p class="opacity-70">Loading…</p>
{/if}
