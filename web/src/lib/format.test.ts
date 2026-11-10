import { describe, expect, it } from 'vitest';
import { formatBytes, formatPercent, formatUptime } from './format';

describe('format', () => {
	it('formats bytes', () => {
		expect(formatBytes(0)).toBe('0 B');
		expect(formatBytes(512)).toBe('512 B');
		expect(formatBytes(1536)).toBe('1.5 KiB');
		expect(formatBytes(8 * 1024 ** 3)).toBe('8.0 GiB');
	});

	it('formats uptime', () => {
		expect(formatUptime(59)).toBe('0m');
		expect(formatUptime(3700)).toBe('1h 1m');
		expect(formatUptime(90061)).toBe('1d 1h 1m');
	});

	it('formats percent defensively', () => {
		expect(formatPercent(12.345)).toBe('12.3%');
		expect(formatPercent(NaN)).toBe('0.0%');
	});
});
