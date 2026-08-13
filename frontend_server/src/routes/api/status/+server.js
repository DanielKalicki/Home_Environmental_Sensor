/**
 * `GET /api/status` — what this server is doing and the newest reading it has.
 *
 * This is the endpoint the dashboard polls for its current-value cards: it is
 * small, of fixed shape, and answered entirely from memory. It describes this
 * server, not the device: `deviceOnline` says whether the last pass reached
 * the device, and the values are the newest ones collected, which stay
 * available while the device is unreachable.
 */
import { json } from '@sveltejs/kit';

import { snapshot } from '$lib/server/poller.js';

export function GET() {
	return json({ now: Date.now(), ...snapshot() }, { headers: { 'cache-control': 'no-store' } });
}
