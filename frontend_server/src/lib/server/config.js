/**
 * Runtime configuration, read from the environment once at startup.
 *
 * Every value has a default so the server starts without a `.env` file; only
 * `DEVICE_URL` normally has to be set, to the address the device printed at
 * boot.
 */
import { env } from '$env/dynamic/private';

/** Base URL of the device's own HTTP server, without a trailing slash. */
export const DEVICE_URL = (env.DEVICE_URL ?? 'http://192.168.1.50').replace(/\/+$/, '');

/** Milliseconds between two `/api/status` polls of the device. */
export const POLL_INTERVAL_MS = positiveNumber(env.POLL_INTERVAL_MS, 5000);

/** Milliseconds to wait for one device response before aborting it. */
export const REQUEST_TIMEOUT_MS = positiveNumber(env.REQUEST_TIMEOUT_MS, 15000);

/** Directory the collected readings are written to. */
export const DATA_DIR = env.DATA_DIR ?? 'data';

/**
 * Readings requested per `/api/readings` call.
 *
 * The device caps a page at 2000, but a page that large is not worth asking
 * for. A reading is written out as JSON, and the BME690's, which carries every
 * BSEC output, comes to about 460 bytes; 2000 of them are a response of nearly
 * a megabyte, sent through the device's 1536-byte transmit buffer, which takes
 * long enough to be at risk of `REQUEST_TIMEOUT_MS`. That matters most on the
 * first pass after this server starts, which is the one pass that asks for a
 * sensor's whole retained history rather than the handful of readings taken
 * since the last pass.
 *
 * At this size the largest response is about 115 KB, which the device has been
 * measured to send in around a second, and a full day of BME690 readings is
 * recovered in under sixty requests.
 */
export const DEVICE_PAGE_LIMIT = 250;

/**
 * Sensors the device exposes, in the order they are shown.
 *
 * Re-exported from the shared definitions rather than listed again here: the
 * poller collects exactly this list, so a sensor named only in `sensors.js`
 * would be charted but never fetched, and the charts would stay empty.
 */
export { SENSORS } from '$lib/sensors.js';

/**
 * Smallest gap, in milliseconds, between a stored reading and a newly pulled
 * one for the new one to count as distinct.
 *
 * After a device reboot the sequence numbers restart and the device serves its
 * whole retained history again, most of which is already stored here. The
 * readings are re-dated against the wall clock rather than matched by sequence
 * number, so they are separated by timestamp, and the tolerance keeps a
 * reading that lands a few milliseconds after its stored twin from being
 * appended twice.
 */
export const DUPLICATE_TOLERANCE_MS = 500;

function positiveNumber(value, fallback) {
	const parsed = Number(value);
	return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}
