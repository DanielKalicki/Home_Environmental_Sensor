/**
 * `GET /api/thermal` — the newest thermal image this server has collected.
 *
 * Answered entirely from memory, like `/api/status`, but kept separate from it
 * because one image is 768 temperatures and the dashboard polls the status far
 * more often than the camera takes a picture.
 *
 * `available` is false until the first image has been fetched from the device,
 * which is the normal state for the first few seconds after either machine
 * starts.
 */
import { json } from '@sveltejs/kit';

import { latestThermal } from '$lib/server/poller.js';

export function GET() {
	return json(
		{ now: Date.now(), ...latestThermal() },
		{ headers: { 'cache-control': 'no-store' } }
	);
}
