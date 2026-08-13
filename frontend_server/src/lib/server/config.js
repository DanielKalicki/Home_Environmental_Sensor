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
 * The device caps a page at 2000 and answers it from a ring buffer in PSRAM,
 * so asking for more only wastes a round trip.
 */
export const DEVICE_PAGE_LIMIT = 2000;

/** Sensors the device exposes, in the order they are shown. */
export const SENSORS = ['scd41', 'sps30', 'bme690', 'as7343'];

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
