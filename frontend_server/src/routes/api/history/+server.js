/**
 * `GET /api/history` — stored readings over a time range, for drawing.
 *
 * | Parameter | Default | Meaning |
 * | --- | --- | --- |
 * | `from` | `to` minus one hour | Start of the range, in milliseconds since the Unix epoch. |
 * | `to` | now | End of the range, the same way. |
 * | `points` | 600 | Largest number of points to return per sensor. |
 * | `sensor` | every sensor | Restrict the answer to one sensor. |
 *
 * The reading and averaging themselves are in `$lib/server/history.js`, which
 * the page's own server-side load uses as well, so the two cannot disagree.
 */
import { error, json } from '@sveltejs/kit';

import { SENSORS } from '$lib/sensors.js';
import { DEFAULT_POINTS, DEFAULT_RANGE_MS, historyRange } from '$lib/server/history.js';

export function GET({ url }) {
	const to = integerParam(url, 'to', Date.now());
	const from = integerParam(url, 'from', to - DEFAULT_RANGE_MS);
	const points = integerParam(url, 'points', DEFAULT_POINTS);

	if (!(from < to)) {
		error(400, 'from must be earlier than to');
	}

	const sensor = url.searchParams.get('sensor');
	if (sensor !== null && !SENSORS.includes(sensor)) {
		error(400, 'unknown sensor');
	}

	return json(historyRange({ from, to, points, sensor }), {
		headers: { 'cache-control': 'no-store' }
	});
}

function integerParam(url, name, fallback) {
	const raw = url.searchParams.get(name);
	if (raw === null) {
		return fallback;
	}

	const value = Number(raw);
	if (!Number.isFinite(value)) {
		error(400, `${name} must be a number`);
	}

	return Math.trunc(value);
}
