export interface Sample {
	ts: number;
	cpu: number;
	memory: number;
	swap: number;
	disk: number;
}

export interface Memory {
	total: number;
	free: number;
	available: number;
	buffers: number;
	cached: number;
	used: number;
	swap_total: number;
	swap_free: number;
	swap_used: number;
}

export interface Disk {
	total: number;
	used: number;
	free: number;
}

export interface SystemInfo {
	hostname: string;
	os: string;
	kernel: string;
	cpu_model: string;
	cpu_cores: number;
	uptime_secs: number;
	time: number;
	processes_total: number;
	processes_running: number;
	load_avg: [number, number, number];
	memory: Memory;
	disk: Disk;
}

export interface Current {
	sample: Sample;
	system: SystemInfo;
}

type Fetch = typeof fetch;

async function getJson<T>(url: string, f: Fetch): Promise<T> {
	const res = await f(url);
	if (!res.ok) throw new Error(`${url} -> HTTP ${res.status}`);
	return (await res.json()) as T;
}

export const fetchCurrent = (f: Fetch = fetch) => getJson<Current>('/api/metrics/current', f);

export const fetchHistory = (minutes = 30, f: Fetch = fetch) =>
	getJson<Sample[]>(`/api/metrics/history?minutes=${minutes}`, f);
