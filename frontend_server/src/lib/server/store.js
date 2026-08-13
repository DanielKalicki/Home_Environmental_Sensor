/**
 * Persistent store for the readings pulled off the device.
 *
 * The device retains one day of readings in PSRAM and overwrites the oldest as
 * it goes. This store exists so the history outlives that window and the
 * device's own uptime: every reading pulled is appended to a file and stays
 * there.
 *
 * One file per sensor per calendar day, holding one JSON object per line
 * (newline-delimited JSON):
 *
 *     data/scd41/2026-08-13.jsonl
 *
 * Appending a line is a single write with no rewriting of what is already
 * there, and a query only has to open the files whose day overlaps the
 * requested range, so neither cost grows with the size of the history. Each
 * stored object is the reading as the device reported it, with `t` added: the
 * wall-clock time in milliseconds since the Unix epoch at which it was taken.
 * The device's own sequence numbers are not stored, because they restart at
 * zero every time it reboots and so cannot order a history that spans one.
 */
import fs from 'node:fs';
import path from 'node:path';

import { DATA_DIR, SENSORS } from './config.js';

/** Newest stored timestamp per sensor, so appends can reject stale readings. */
const lastTimestamps = new Map();

/** Newest stored reading per sensor, to answer "current value" without a read. */
const latestReadings = new Map();

/**
 * Reject a sensor name that is not one the device serves.
 *
 * Sensor names reach this module from query strings and are used to build file
 * paths, so they are checked against the fixed list rather than sanitised: a
 * name like `../../etc/passwd` must not be able to name a file at all.
 */
function assertKnownSensor(sensor) {
	if (!SENSORS.includes(sensor)) {
		throw new Error(`unknown sensor "${sensor}"`);
	}
}

/** Directory holding one sensor's daily files. */
function sensorDirectory(sensor) {
	return path.join(DATA_DIR, sensor);
}

/** `YYYY-MM-DD` in UTC, which is what names a daily file. */
function dayKey(timestamp) {
	return new Date(timestamp).toISOString().slice(0, 10);
}

/** Path of the file holding the readings of one sensor on one day. */
function dayFile(sensor, timestamp) {
	return path.join(sensorDirectory(sensor), `${dayKey(timestamp)}.jsonl`);
}

/** Names of a sensor's daily files, oldest first. */
function dayFiles(sensor) {
	try {
		return fs
			.readdirSync(sensorDirectory(sensor))
			.filter((name) => name.endsWith('.jsonl'))
			.sort();
	} catch (error) {
		if (error.code === 'ENOENT') {
			return [];
		}
		throw error;
	}
}

/** Every reading in one daily file, oldest first. */
function readDayFile(sensor, fileName) {
	let contents;
	try {
		contents = fs.readFileSync(path.join(sensorDirectory(sensor), fileName), 'utf8');
	} catch (error) {
		if (error.code === 'ENOENT') {
			return [];
		}
		throw error;
	}

	const readings = [];
	for (const line of contents.split('\n')) {
		if (line === '') {
			continue;
		}
		try {
			readings.push(JSON.parse(line));
		} catch {
			// A line can only be incomplete if the process was killed mid-write,
			// which can affect the last line of the newest file at most. Skipping
			// it costs one reading and keeps the rest of the day readable.
		}
	}
	return readings;
}

/**
 * Load what is already on disk into the in-memory summaries.
 *
 * Only the newest daily file of each sensor is read: all that is needed is the
 * newest reading and its timestamp, and every older file is by definition
 * older than that.
 */
export function initialize() {
	fs.mkdirSync(DATA_DIR, { recursive: true });

	for (const sensor of SENSORS) {
		fs.mkdirSync(sensorDirectory(sensor), { recursive: true });

		const files = dayFiles(sensor);
		if (files.length === 0) {
			continue;
		}

		const readings = readDayFile(sensor, files[files.length - 1]);
		const newest = readings[readings.length - 1];
		if (newest) {
			lastTimestamps.set(sensor, newest.t);
			latestReadings.set(sensor, newest);
		}
	}
}

/** Wall-clock time of the newest stored reading of `sensor`, or 0 if none. */
export function lastTimestamp(sensor) {
	assertKnownSensor(sensor);
	return lastTimestamps.get(sensor) ?? 0;
}

/** The newest stored reading of `sensor`, or `null` if there is none. */
export function latestReading(sensor) {
	assertKnownSensor(sensor);
	return latestReadings.get(sensor) ?? null;
}

/**
 * Append readings to a sensor's history, oldest first.
 *
 * Readings that are not newer than what is already stored are dropped, which
 * is what keeps the history from gaining a second copy of everything each time
 * the device reboots and serves its retained day over again.
 *
 * @param {string} sensor
 * @param {Array<{t: number}>} readings Sorted by `t`, oldest first.
 * @returns {number} How many were actually written.
 */
export function append(sensor, readings) {
	assertKnownSensor(sensor);

	let newest = lastTimestamps.get(sensor) ?? 0;
	let newestReading = latestReadings.get(sensor) ?? null;
	let written = 0;

	// Group consecutive readings by the day they fall in, so a page that does
	// not cross midnight is written with a single open-append-close.
	let pendingFile = null;
	let pendingLines = [];

	const flush = () => {
		if (pendingFile === null || pendingLines.length === 0) {
			return;
		}
		fs.appendFileSync(pendingFile, pendingLines.join(''));
		pendingFile = null;
		pendingLines = [];
	};

	for (const reading of readings) {
		if (!(reading.t > newest)) {
			continue;
		}

		const file = dayFile(sensor, reading.t);
		if (file !== pendingFile) {
			flush();
			pendingFile = file;
		}
		pendingLines.push(`${JSON.stringify(reading)}\n`);

		newest = reading.t;
		newestReading = reading;
		written += 1;
	}

	flush();

	if (written > 0) {
		lastTimestamps.set(sensor, newest);
		latestReadings.set(sensor, newestReading);
	}

	return written;
}

/**
 * Every stored reading of `sensor` taken in `[from, to]`, oldest first.
 *
 * Only the daily files whose day overlaps the range are opened.
 */
export function readRange(sensor, from, to) {
	assertKnownSensor(sensor);

	const firstDay = dayKey(from);
	const lastDay = dayKey(to);
	const readings = [];

	for (const fileName of dayFiles(sensor)) {
		const day = fileName.slice(0, -'.jsonl'.length);
		if (day < firstDay || day > lastDay) {
			continue;
		}
		for (const reading of readDayFile(sensor, fileName)) {
			if (reading.t >= from && reading.t <= to) {
				readings.push(reading);
			}
		}
	}

	return readings;
}

/** Wall-clock time of the oldest stored reading of any sensor, or `null`. */
export function oldestTimestamp() {
	let oldest = null;

	for (const sensor of SENSORS) {
		const files = dayFiles(sensor);
		if (files.length === 0) {
			continue;
		}
		const first = readDayFile(sensor, files[0])[0];
		if (first && (oldest === null || first.t < oldest)) {
			oldest = first.t;
		}
	}

	return oldest;
}
