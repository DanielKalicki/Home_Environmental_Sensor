/**
 * Background poller: the part of this server that talks to the device.
 *
 * It runs on its own timer, independently of any browser: readings are
 * collected whether or not the dashboard is open, so the history is continuous
 * as long as this server is running. Nothing in the browser ever contacts the
 * device.
 *
 * Each pass asks the device for `/api/status`, works out which sequence
 * numbers it has not pulled yet, and fetches exactly those with
 * `/api/readings`. On the first pass it takes the device's whole retained
 * history, which is a day's worth.
 */
import {
	DEVICE_PAGE_LIMIT,
	DEVICE_URL,
	DUPLICATE_TOLERANCE_MS,
	POLL_INTERVAL_MS,
	SENSORS
} from './config.js';
import { abortInFlight, fetchReadings, fetchStatus } from './device.js';
import * as store from './store.js';

/** Sequence number each sensor should be pulled from next. */
const cursors = new Map();

/** Snapshot of what the poller is doing, served by `/api/status`. */
const state = {
	deviceUrl: DEVICE_URL,
	pollIntervalMs: POLL_INTERVAL_MS,
	running: false,
	deviceOnline: false,
	lastPollAt: null,
	lastSuccessAt: null,
	lastError: null,
	deviceUptimeMs: null,
	deviceWindowMs: null,
	rebootsSeen: 0,
	sensors: {}
};

for (const sensor of SENSORS) {
	state.sensors[sensor] = {
		intervalMs: null,
		deviceRetained: null,
		storedTotal: 0,
		lastReadingAt: null
	};
}

let timer = null;
let wakeUp = null;
let started = false;

/** Start the poller. Calling it more than once has no effect. */
export function start() {
	if (started) {
		return;
	}
	started = true;

	store.initialize();
	for (const sensor of SENSORS) {
		state.sensors[sensor].lastReadingAt = store.lastTimestamp(sensor) || null;
	}

	state.running = true;
	console.log(`poller: following ${DEVICE_URL} every ${POLL_INTERVAL_MS} ms`);
	void loop();
}

/**
 * Stop the poller.
 *
 * It returns having released everything that could keep the Node event loop
 * alive: the wait between passes is cancelled and any request still waiting on
 * the device is aborted. Without that, a process asked to shut down would
 * carry on running until the next pass was due, which is what makes a server
 * ignore Ctrl-C.
 */
export function stop() {
	started = false;
	state.running = false;

	if (timer !== null) {
		clearTimeout(timer);
		timer = null;
	}

	// End the wait between passes now rather than letting it run out, so the
	// loop sees `started` is false and returns immediately.
	if (wakeUp !== null) {
		const resolve = wakeUp;
		wakeUp = null;
		resolve();
	}

	abortInFlight();
}

/** The current poller state, plus each sensor's newest reading. */
export function snapshot() {
	const sensors = {};
	for (const sensor of SENSORS) {
		sensors[sensor] = { ...state.sensors[sensor], latest: store.latestReading(sensor) };
	}

	return { ...state, sensors, oldestReadingAt: store.oldestTimestamp() };
}

async function loop() {
	while (started) {
		try {
			await pollOnce();

			// Only the change of state is worth a line. Saying the device is
			// reachable every few seconds tells nobody anything, and saying it is
			// unreachable every few seconds buries whatever else is in the console.
			if (state.lastError !== null) {
				console.log('poller: device reachable again');
			}

			state.deviceOnline = true;
			state.lastError = null;
			state.lastSuccessAt = Date.now();
		} catch (error) {
			const message = error instanceof Error ? error.message : String(error);

			if (state.deviceOnline || state.lastError !== message) {
				console.warn(`poller: ${message}`);
			}

			state.deviceOnline = false;
			state.lastError = message;
		}

		state.lastPollAt = Date.now();

		if (!started) {
			return;
		}
		await new Promise((resolve) => {
			wakeUp = resolve;
			timer = setTimeout(() => {
				wakeUp = null;
				resolve();
			}, POLL_INTERVAL_MS);
			// The wait must not be a reason for the process to stay alive: while
			// the HTTP server is listening it keeps the process up on its own, and
			// once that server closes there is nothing left worth waiting for.
			timer.unref?.();
		});
	}
}

async function pollOnce() {
	const status = await fetchStatus();

	// A device uptime lower than the one seen last pass means the device
	// rebooted: its sequence numbers restarted at zero, so the cursors point at
	// readings that no longer exist and every sensor has to be picked up from
	// the start of its retained history again. The readings already stored here
	// survive; the overlap is dropped by timestamp when it is appended.
	if (state.deviceUptimeMs !== null && status.uptime_ms < state.deviceUptimeMs) {
		cursors.clear();
		state.rebootsSeen += 1;
		console.log('poller: device rebooted, restarting from its oldest retained reading');
	}

	state.deviceUptimeMs = status.uptime_ms;
	state.deviceWindowMs = status.window_ms;

	for (const sensor of SENSORS) {
		const sensorStatus = status.sensors?.[sensor];
		if (!sensorStatus) {
			continue;
		}

		state.sensors[sensor].intervalMs = sensorStatus.interval_ms;
		state.sensors[sensor].deviceRetained = sensorStatus.len;

		// A capacity of zero means the device could not reserve its PSRAM ring
		// buffer, so it retains nothing and there is nothing to ask for.
		if (sensorStatus.capacity === 0) {
			continue;
		}

		await syncSensor(sensor, sensorStatus);
	}
}

async function syncSensor(sensor, sensorStatus) {
	// Readings older than `first_sequence` have been overwritten on the device
	// and cannot be recovered, so a cursor that has fallen behind is moved up
	// rather than retried.
	let from = Math.max(cursors.get(sensor) ?? sensorStatus.first_sequence, sensorStatus.first_sequence);

	while (from < sensorStatus.next_sequence) {
		const page = await fetchReadings(sensor, from, DEVICE_PAGE_LIMIT);
		if (!Array.isArray(page.readings) || page.readings.length === 0) {
			break;
		}

		const written = store.append(sensor, datePage(page));
		state.sensors[sensor].storedTotal += written;
		state.sensors[sensor].lastReadingAt = store.lastTimestamp(sensor) || null;

		// `page.from` is the requested `from` raised to whatever the device
		// still holds, so following it rather than the request keeps the cursor
		// correct when readings were lost to overwriting.
		from = page.from + page.readings.length;
	}

	cursors.set(sensor, from);
}

/**
 * Put one page of readings on the wall-clock timeline.
 *
 * The device has no real-time clock: it dates a reading by the uptime at which
 * it was taken, and reports its uptime at the moment it answered. The
 * difference between the two is how long before the response the reading was
 * taken, which subtracted from the local time the response arrived gives the
 * wall-clock time it was taken. Every reading of a page is dated against that
 * one response, so readings in a page keep their exact spacing and only the
 * network round trip, tens of milliseconds, is added to all of them alike.
 *
 * The device sends `null` for a reading it overwrote while the response was
 * being written; there is nothing to store for those.
 */
function datePage(page) {
	const stored = [];
	const alreadyStored = store.lastTimestamp(page.sensor);

	for (const reading of page.readings) {
		if (reading === null || typeof reading !== 'object') {
			continue;
		}

		const { taken_at_ms: takenAtMs, ...values } = reading;
		const t = page.receivedAt - (page.uptime_ms - takenAtMs);

		if (t <= alreadyStored + DUPLICATE_TOLERANCE_MS) {
			continue;
		}

		stored.push({ t, device_uptime_ms: takenAtMs, ...values });
	}

	return stored;
}
