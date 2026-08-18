/**
 * Client for the device's read-only HTTP API.
 *
 * The device serves `GET /api/status`, `GET /api/readings` and
 * `GET /api/thermal` (see `software/README.md`). All are plain JSON over
 * HTTP/1.1; `/api/readings` and `/api/thermal` have no `Content-Length` and
 * end when the device closes the connection, which `fetch` handles on its own.
 */
import { DEVICE_URL, DEVICE_PAGE_LIMIT, REQUEST_TIMEOUT_MS } from './config.js';

/**
 * The requests currently in flight, so shutdown can cancel them.
 *
 * A request that is still waiting on the device holds an open socket, and an
 * open socket keeps the Node event loop alive, which would leave the process
 * running for up to `REQUEST_TIMEOUT_MS` after it was asked to stop.
 */
const inFlight = new Set();

/**
 * Quiet time between one request finishing and the next one starting.
 *
 * The device serves a single connection at a time: it accepts one, answers it,
 * closes it, and only then goes back to accepting. Nothing is listening on
 * port 80 during that turnaround, so a connection attempted in that window is
 * refused outright. Leaving a gap between requests keeps the client out of it
 * instead of racing the device for its own socket.
 */
const REQUEST_GAP_MS = 150;

/**
 * Waits before each retry of a request that failed to connect, in
 * milliseconds. A refused connection means the device was between
 * connections, not that it is gone, so it is worth trying again shortly; the
 * waits lengthen so a device that really is unreachable is not hammered.
 */
const RETRY_DELAYS_MS = [200, 500, 1200, 2500];

/** True while a request is in progress, so the next one waits its turn. */
let queue = Promise.resolve();

/** Cancel every request in flight; they reject as if they had timed out. */
export function abortInFlight() {
	for (const controller of inFlight) {
		controller.abort();
	}
	inFlight.clear();
}

const sleep = (ms) =>
	new Promise((resolve) => {
		const timer = setTimeout(resolve, ms);
		timer.unref?.();
	});

/**
 * A failure worth retrying: the connection never got as far as a response.
 *
 * `ECONNREFUSED` is the device between connections, `ECONNRESET` and
 * `UND_ERR_SOCKET` are it dropping one, `EHOSTUNREACH` and `ETIMEDOUT` are a
 * device that has not come back to the network yet. A response that arrived
 * and said something unwelcome is not retried here: that is not a transport
 * problem and the next poll will pick it up anyway.
 */
function isTransient(error) {
	if (error?.name === 'AbortError') {
		return false;
	}

	const code = error?.cause?.code ?? error?.code;
	return ['ECONNREFUSED', 'ECONNRESET', 'EHOSTUNREACH', 'ETIMEDOUT', 'UND_ERR_SOCKET'].includes(
		code
	);
}

/**
 * A message that says what actually went wrong.
 *
 * Node reports every transport failure from `fetch` as the bare text "fetch
 * failed" and puts the reason in `cause`, which on its own tells nobody
 * anything.
 */
function describe(error, pathAndQuery) {
	if (error?.name === 'AbortError') {
		return `${pathAndQuery} timed out after ${REQUEST_TIMEOUT_MS} ms`;
	}

	const cause = error?.cause;
	if (cause?.code) {
		return `${pathAndQuery}: ${cause.code} (${cause.message ?? error.message})`;
	}

	return `${pathAndQuery}: ${error?.message ?? String(error)}`;
}

/** One attempt at a request, with its own timeout. */
async function attempt(pathAndQuery) {
	const controller = new AbortController();
	inFlight.add(controller);

	const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
	// The timeout itself must not keep the process alive either: it exists to
	// cut short a request, not to give the process a reason to stay running.
	timer.unref?.();

	try {
		const response = await fetch(`${DEVICE_URL}${pathAndQuery}`, {
			signal: controller.signal,
			headers: { accept: 'application/json' }
		});

		if (!response.ok) {
			throw new Error(`device answered ${response.status}`);
		}

		return await response.json();
	} finally {
		clearTimeout(timer);
		inFlight.delete(controller);
	}
}

/**
 * Fetch a device resource and parse its JSON body.
 *
 * Requests are run strictly one after another, separated by `REQUEST_GAP_MS`,
 * because the device can only serve one at a time and a second connection
 * opened while it is busy is simply refused. A refusal that happens anyway is
 * retried a few times before the request is given up on.
 */
function getJson(pathAndQuery) {
	// Chain onto whatever is already queued, so two callers cannot have
	// requests open at the same time. The chain is kept unbroken by swallowing
	// the result here; the caller still gets the real outcome.
	const result = queue.then(() => run(pathAndQuery));
	queue = result.then(
		() => sleep(REQUEST_GAP_MS),
		() => sleep(REQUEST_GAP_MS)
	);
	return result;
}

async function run(pathAndQuery) {
	let lastError;

	for (let attemptIndex = 0; attemptIndex <= RETRY_DELAYS_MS.length; attemptIndex += 1) {
		try {
			return await attempt(pathAndQuery);
		} catch (error) {
			lastError = error;

			if (!isTransient(error) || attemptIndex === RETRY_DELAYS_MS.length) {
				break;
			}

			await sleep(RETRY_DELAYS_MS[attemptIndex]);
		}
	}

	throw new Error(describe(lastError, pathAndQuery), { cause: lastError });
}

/**
 * The device's uptime and the state of every history.
 *
 * @returns {Promise<{uptime_ms: number, window_ms: number, sensors: Record<string, {
 *   interval_ms: number, capacity: number, len: number,
 *   first_sequence: number, next_sequence: number }>}>}
 */
export function fetchStatus() {
	return getJson('/api/status');
}

/**
 * One page of a single sensor's readings, oldest first.
 *
 * `receivedAt` is the local wall-clock time the response arrived. The device
 * has no real-time clock and dates its readings by its own uptime, so this is
 * what lets a reading be placed on a wall-clock timeline: the difference
 * between the response's `uptime_ms` and a reading's `taken_at_ms` is how long
 * before the response that reading was taken.
 *
 * @param {string} sensor One of the names in `SENSORS`.
 * @param {number} from Lowest sequence number wanted.
 * @param {number} limit Largest number of readings to return.
 */
export async function fetchReadings(sensor, from, limit = DEVICE_PAGE_LIMIT) {
	const query = new URLSearchParams({
		sensor,
		from: String(from),
		limit: String(Math.min(limit, DEVICE_PAGE_LIMIT))
	});

	const page = await getJson(`/api/readings?${query}`);
	return { ...page, receivedAt: Date.now() };
}

/**
 * The newest thermal image the camera has taken.
 *
 * The device keeps no history of images, only the last one, so this is always
 * a single image and there is nothing to page through. `sequence` counts the
 * images taken since the device booted, which is what tells a new image from
 * one already held.
 *
 * `receivedAt` is added for the same reason as in `fetchReadings`: the device
 * dates the image by its own uptime, and only the local time the response
 * arrived can place it on a wall clock.
 *
 * @returns {Promise<{uptime_ms: number, available: boolean, width: number,
 *   height: number, pixels: number[], receivedAt: number,
 *   taken_at_ms?: number, sequence?: number, interval_ms?: number,
 *   min_celsius?: number, max_celsius?: number, mean_celsius?: number,
 *   ambient_celsius?: number}>}
 */
export async function fetchThermal() {
	const image = await getJson('/api/thermal');
	return { ...image, receivedAt: Date.now() };
}
