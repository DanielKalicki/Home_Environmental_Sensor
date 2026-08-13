/**
 * Reading a range of stored history in the shape the charts want.
 *
 * Both the `/api/history` endpoint and the page's own server-side load use
 * this, so what the page is first rendered with and what it later fetches for
 * itself are produced by exactly the same code.
 */
import { SENSORS, chartedFields } from '$lib/sensors.js';

import { readRange } from './store.js';

/** Largest number of buckets a request may ask for. */
export const MAX_POINTS = 5000;
/** Buckets returned when the request does not say. */
export const DEFAULT_POINTS = 600;
/** Range covered when the request does not say, in milliseconds. */
export const DEFAULT_RANGE_MS = 60 * 60 * 1000;

/**
 * Stored readings over a time range, ready to draw.
 *
 * A range of a week holds far more readings than a chart a few hundred pixels
 * wide can show, so a range holding more than `points` readings is averaged
 * into that many equal buckets. The result therefore has a bounded size
 * whatever range is asked for, and a bucket's value is the mean of the
 * readings in it rather than one reading picked out of it, so a spike between
 * two sampled points cannot vanish entirely.
 *
 * @param {{from: number, to: number, points?: number, sensor?: string | null}} request
 */
export function historyRange({ from, to, points = DEFAULT_POINTS, sensor = null }) {
	const bucketCount = Math.min(Math.max(Math.trunc(points), 1), MAX_POINTS);
	const wanted = sensor === null ? SENSORS : [sensor];
	const sensors = {};

	for (const name of wanted) {
		sensors[name] = downsample(readRange(name, from, to), chartedFields(name), from, to, bucketCount);
	}

	return { from, to, points: bucketCount, sensors };
}

/**
 * Average `readings` into at most `points` equally spaced buckets.
 *
 * Only `fields` are kept, and a field is averaged over the readings of its
 * bucket that actually carry it. An array field, which is how the AS7343
 * reports its three integration cycles, is averaged element by element so the
 * three stay distinguishable. A bucket with no readings produces no point, so
 * a gap in the history stays a gap in the chart instead of being bridged by a
 * straight line.
 */
function downsample(readings, fields, from, to, points) {
	if (readings.length === 0) {
		return [];
	}

	if (readings.length <= points) {
		return readings.map((reading) => project(reading, fields));
	}

	const bucketMs = (to - from) / points;
	const output = [];

	let bucketIndex = 0;
	let bucket = [];

	const flush = () => {
		if (bucket.length === 0) {
			return;
		}
		output.push(average(bucket, fields));
		bucket = [];
	};

	for (const reading of readings) {
		const index = Math.min(Math.floor((reading.t - from) / bucketMs), points - 1);
		if (index !== bucketIndex) {
			flush();
			bucketIndex = index;
		}
		bucket.push(reading);
	}
	flush();

	return output;
}

/** One reading reduced to `t` and the fields the charts read. */
function project(reading, fields) {
	const point = { t: reading.t };
	for (const field of fields) {
		const value = reading[field];
		if (value !== undefined && value !== null) {
			point[field] = value;
		}
	}
	return point;
}

/** The mean of one bucket of readings, field by field. */
function average(bucket, fields) {
	const point = { t: Math.round(bucket.reduce((sum, r) => sum + r.t, 0) / bucket.length) };

	for (const field of fields) {
		let count = 0;
		let total = null;

		for (const reading of bucket) {
			const value = reading[field];

			if (Array.isArray(value)) {
				if (total === null) {
					total = value.map(() => 0);
				}
				for (let i = 0; i < total.length && i < value.length; i += 1) {
					total[i] += value[i];
				}
				count += 1;
			} else if (typeof value === 'number' && Number.isFinite(value)) {
				total = (total ?? 0) + value;
				count += 1;
			}
		}

		if (count === 0) {
			continue;
		}

		point[field] = Array.isArray(total) ? total.map((sum) => sum / count) : total / count;
	}

	return point;
}
