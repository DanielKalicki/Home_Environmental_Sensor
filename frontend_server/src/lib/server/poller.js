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
import { abortInFlight, fetchReadings, fetchStatus, fetchThermal } from './device.js';
import * as store from './store.js';

/** Sequence number each sensor should be pulled from next. */
const cursors = new Map();

/**
 * The newest thermal image, or `null` before one has been fetched.
 *
 * Images are held in memory only and never written to disk. The device keeps
 * just the last one, so there is no history to collect, and one image is 768
 * temperatures: storing them at the camera's rate would outgrow every other
 * sensor's history put together within a day.
 */
let thermal = null;

/** Why the last thermal fetch failed, or `null` if it succeeded. */
let thermalError = null;

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
		lastReadingAt: null,
		lastError: null
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

/**
 * The newest thermal image, dated against the wall clock.
 *
 * It is deliberately not part of `snapshot`: the dashboard asks for the
 * status several times a minute, and 768 temperatures do not belong in a
 * response that small. `takenAt` is the local time the image was taken,
 * worked out the same way a reading's timestamp is.
 */
export function latestThermal() {
	if (thermal === null) {
		return { available: false, error: thermalError };
	}

	return {
		available: true,
		error: thermalError,
		takenAt: thermal.receivedAt - (thermal.uptime_ms - thermal.taken_at_ms),
		sequence: thermal.sequence,
		intervalMs: thermal.interval_ms ?? null,
		width: thermal.width,
		height: thermal.height,
		minCelsius: thermal.min_celsius,
		maxCelsius: thermal.max_celsius,
		meanCelsius: thermal.mean_celsius,
		ambientCelsius: thermal.ambient_celsius,
		pixels: thermal.pixels
	};
}

/**
 * Record that the device answered.
 *
 * This is called the moment `/api/status` comes back, not when the pass that
 * asked for it finishes. A pass has to catch up on every reading taken while
 * this server was not running, and that can take minutes; a device marked
 * reachable only at the end of it would be reported as unreachable for the
 * whole of a backfill it is plainly answering.
 *
 * Only the change of state is worth a line. Saying the device is reachable
 * every few seconds tells nobody anything, and saying it is unreachable every
 * few seconds buries whatever else is in the console.
 */
function markReachable() {
	if (state.lastError !== null) {
		console.log('poller: device reachable again');
	}

	state.deviceOnline = true;
	state.lastError = null;
	state.lastSuccessAt = Date.now();
}

async function loop() {
	while (started) {
		try {
			await pollOnce();
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

	// The device has answered, so it is reachable now, whatever the rest of
	// this pass turns out to cost.
	markReachable();

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

	// Fetched before the sensors rather than after: a sensor's history can be
	// a whole day of readings paged 250 at a time, which can take minutes to
	// catch up on. The thermal image is a single cheap request with nothing to
	// page through, and the camera takes a new one every 10 s regardless, so
	// it must not be left waiting behind that backlog — otherwise the image
	// shown is however old the last full pass was, not the camera's current
	// frame.
	await syncThermal();

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

		// One sensor's failure must not cost the others their pass. The sensors
		// are pulled in a fixed order, so without this a sensor that cannot be
		// fetched stops every sensor after it from being fetched at all, for as
		// long as it keeps failing — and because its own cursor never advances,
		// that is forever. The result is a history that keeps growing for the
		// first sensors in the list and is frozen for the rest.
		try {
			await syncSensor(sensor, sensorStatus);

			if (state.sensors[sensor].lastError !== null) {
				console.log(`poller: ${sensor} readable again`);
				state.sensors[sensor].lastError = null;
			}
		} catch (error) {
			const message = error instanceof Error ? error.message : String(error);

			if (state.sensors[sensor].lastError !== message) {
				console.warn(`poller: ${sensor}: ${message}`);
				state.sensors[sensor].lastError = message;
			}
		}
	}
}

/**
 * Fetch the newest thermal image, if the camera has taken a new one.
 *
 * A failure is recorded rather than thrown: the camera is one device among
 * several, and an image that could not be fetched must not cost the pass its
 * readings or mark the whole device unreachable.
 */
async function syncThermal() {
	try {
		const image = await fetchThermal();

		if (image.available) {
			// The device restarts its sequence at zero when it reboots, so an
			// image whose number went backwards is a new image too.
			thermal = image;
		} else if (thermal !== null && image.uptime_ms < thermal.uptime_ms) {
			// The device rebooted and has not finished its first image yet; the
			// one held here belongs to the previous run.
			thermal = null;
		}

		if (thermalError !== null) {
			console.log('poller: thermal camera readable again');
			thermalError = null;
		}
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);

		if (thermalError !== message) {
			console.warn(`poller: thermal: ${message}`);
			thermalError = message;
		}
	}
}

async function syncSensor(sensor, sensorStatus) {
	// With no cursor yet — the first pass after this server started, or the
	// first after the device rebooted and renumbered everything — the place to
	// start has to be found rather than assumed.
	const known = cursors.get(sensor);
	const start = known === undefined ? await seekCursor(sensor, sensorStatus) : known;

	// Readings older than `first_sequence` have been overwritten on the device
	// and cannot be recovered, so a cursor that has fallen behind is moved up
	// rather than retried.
	let from = Math.max(start, sensorStatus.first_sequence);

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
 * The first sequence number of `sensor` this server does not already hold.
 *
 * Starting instead from the device's oldest retained reading is what made a
 * restart so slow. The device retains a day, so that is a day of readings
 * pulled a page at a time over a link that manages roughly a hundred kilobytes
 * a second — minutes of transfers for every sensor, all but the last few
 * readings of which are already on disk and are thrown away by `datePage` the
 * moment they arrive. Restarting this server a minute after stopping it should
 * cost a minute of readings, not a day of them.
 *
 * The device numbers its readings in order, so "already stored here" is true
 * of an unbroken run of the oldest sequence numbers and false of every one
 * after it. That boundary is found by asking for single readings and comparing
 * their timestamps against the newest one stored, which is a handful of very
 * small requests instead of a full history.
 *
 * The search walks back from the newest reading in doubling steps before it
 * halves the interval it lands in, because the boundary is usually within a
 * few readings of the newest: the common case is this server having been
 * restarted, not having been away for a day. A pass with nothing new to
 * collect costs a single request, one that missed a few readings costs two or
 * three, and a full day's gap costs under twenty — against the seventy full
 * pages that same gap used to cost.
 */
async function seekCursor(sensor, sensorStatus) {
	const stored = store.lastTimestamp(sensor);
	const first = sensorStatus.first_sequence;
	const next = sensorStatus.next_sequence;

	// Nothing stored for this sensor yet, so every retained reading is wanted.
	if (stored === 0 || next <= first) {
		return first;
	}

	// `low` is not yet known to be wanted; `high` is known to be. A sequence
	// the device does not have counts as wanted, which is what makes `next` a
	// valid upper bound to start from.
	let low = first;
	let high = next;

	for (let step = 1; next - step > first; step *= 2) {
		const candidate = next - step;

		if (await isWanted(sensor, candidate, stored)) {
			high = candidate;
		} else {
			low = candidate + 1;
			break;
		}
	}

	while (low < high) {
		const middle = low + Math.floor((high - low) / 2);

		if (await isWanted(sensor, middle, stored)) {
			high = middle;
		} else {
			low = middle + 1;
		}
	}

	return low;
}

/**
 * Whether the reading at `sequence` is newer than the newest one stored.
 *
 * A reading the device no longer has was taken before everything it still
 * holds, so it is not wanted; a sequence the device has not reached counts as
 * wanted, so the search never walks past the end of the history.
 */
async function isWanted(sensor, sequence, stored) {
	const page = await fetchReadings(sensor, sequence, 1);
	const reading = page.readings?.[0];

	if (reading === undefined) {
		return true;
	}

	if (reading === null || typeof reading.taken_at_ms !== 'number') {
		return false;
	}

	return page.receivedAt - (page.uptime_ms - reading.taken_at_ms) > stored + DUPLICATE_TOLERANCE_MS;
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
