import { describe, expect, it } from 'vitest';
import { areaPath, linePath, scalePoints } from './chart';

const s = (ts: number, cpu: number) => ({ ts, cpu, memory: 0, swap: 0, disk: 0 });

describe('chart', () => {
	it('scales samples into the box and drops out-of-window ones', () => {
		const pts = scalePoints(
			[s(0, 50), s(100, 100), s(200, 0), s(999, 10)],
			'cpu',
			100,
			200,
			200,
			100
		);
		expect(pts).toEqual([
			{ x: 0, y: 0 },
			{ x: 200, y: 100 }
		]);
	});

	it('clamps out-of-range values', () => {
		expect(scalePoints([s(1, 250)], 'cpu', 0, 1, 10, 10)[0].y).toBe(0);
	});

	it('builds paths', () => {
		const pts = [
			{ x: 0, y: 10 },
			{ x: 5, y: 0 }
		];
		expect(linePath(pts)).toBe('M0.0 10.0 L5.0 0.0');
		expect(areaPath(pts, 10)).toBe('M0.0 10.0 L5.0 0.0 L5.0 10 L0.0 10 Z');
		expect(areaPath([], 10)).toBe('');
	});
});
