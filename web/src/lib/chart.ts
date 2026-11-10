import type { Sample } from './api';

export interface Point {
	x: number;
	y: number;
}

/** Map samples onto a width x height box (y inverted, 0..100 %) over a time window. */
export function scalePoints(
	samples: Sample[],
	key: 'cpu' | 'memory' | 'swap' | 'disk',
	from: number,
	to: number,
	width: number,
	height: number
): Point[] {
	const span = Math.max(1, to - from);
	return samples
		.filter((s) => s.ts >= from && s.ts <= to)
		.map((s) => ({
			x: ((s.ts - from) / span) * width,
			y: height - (Math.min(100, Math.max(0, s[key])) / 100) * height
		}));
}

export function linePath(points: Point[]): string {
	return points.map((p, i) => `${i ? 'L' : 'M'}${p.x.toFixed(1)} ${p.y.toFixed(1)}`).join(' ');
}

export function areaPath(points: Point[], height: number): string {
	if (!points.length) return '';
	const first = points[0];
	const last = points[points.length - 1];
	return `${linePath(points)} L${last.x.toFixed(1)} ${height} L${first.x.toFixed(1)} ${height} Z`;
}
