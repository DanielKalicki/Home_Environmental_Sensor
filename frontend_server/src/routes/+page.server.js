/**
 * What the page is rendered with before it starts fetching for itself.
 *
 * Without this the first paint would be an empty dashboard that fills in a
 * moment later. Both values are read straight out of this server's own memory
 * and files, so producing them costs nothing and the page arrives already
 * drawn.
 */
import { DEFAULT_RANGE_MS, historyRange } from '$lib/server/history.js';
import { snapshot } from '$lib/server/poller.js';

/** Points per sensor in the first render; matches what the page asks for. */
const INITIAL_POINTS = 700;

export function load() {
	const to = Date.now();

	return {
		status: { now: to, ...snapshot() },
		history: historyRange({ from: to - DEFAULT_RANGE_MS, to, points: INITIAL_POINTS })
	};
}
