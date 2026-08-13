/**
 * Server startup and shutdown.
 *
 * SvelteKit evaluates this module once when the server process starts, which
 * is where the poller is launched: collection begins with the process, not
 * with the first request, so the history has no gap while nobody is looking at
 * the dashboard.
 */
import { start, stop } from '$lib/server/poller.js';

start();

/**
 * How long to wait after a stop signal before ending the process outright.
 *
 * The handler first stops the poller and lets the HTTP server close its
 * connections on its own, which is enough in the ordinary case. This is the
 * backstop for when something else is still holding the process open, so
 * Ctrl-C always ends it rather than appearing to be ignored.
 */
const SHUTDOWN_GRACE_MS = 3000;

let shuttingDown = false;

function shutdown(signal) {
	// A second Ctrl-C means the wait is unwelcome: end the process at once.
	if (shuttingDown) {
		process.exit(0);
	}
	shuttingDown = true;

	console.log(`\n${signal} received, stopping the poller`);
	stop();

	const forceExit = setTimeout(() => process.exit(0), SHUTDOWN_GRACE_MS);
	// Unreferenced, so it only fires if the process is still running when it
	// comes due; if everything else has already shut down the process exits
	// straight away and this timer never delays it.
	forceExit.unref?.();
}

for (const signal of ['SIGINT', 'SIGTERM']) {
	process.on(signal, () => shutdown(signal));
}
