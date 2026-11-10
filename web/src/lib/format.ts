const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];

export function formatBytes(bytes: number): string {
	if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
	const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), UNITS.length - 1);
	const v = bytes / 1024 ** i;
	return `${v >= 100 || i === 0 ? v.toFixed(0) : v.toFixed(1)} ${UNITS[i]}`;
}

export function formatUptime(secs: number): string {
	const s = Math.max(0, Math.floor(secs));
	const d = Math.floor(s / 86400);
	const h = Math.floor((s % 86400) / 3600);
	const m = Math.floor((s % 3600) / 60);
	const parts = [];
	if (d) parts.push(`${d}d`);
	if (d || h) parts.push(`${h}h`);
	parts.push(`${m}m`);
	return parts.join(' ');
}

export function formatPercent(v: number): string {
	return `${(Number.isFinite(v) ? v : 0).toFixed(1)}%`;
}

export function formatTime(unixSecs: number): string {
	return new Date(unixSecs * 1000).toLocaleString();
}
