/**
 * The horizontal time axis the charts share.
 *
 * Every chart on the dashboard draws the same range of time, so they must
 * label it the same way: a tick at 12:30 on one chart has to sit at exactly
 * the same place on the one beside it. Keeping the tick placement here rather
 * than inside one component is what guarantees that, and it lets the spectrum
 * chart line its columns up with the line charts above it.
 */

export const DAY_MS = 86400000;

/**
 * The steps the axis is allowed to use, in milliseconds. A label therefore
 * always falls on a whole minute, quarter hour, hour and so on, rather than on
 * whatever moment the range happens to begin at. Anything coarser than the
 * last entry is counted in whole days instead, because days are not all the
 * same length once the clocks change.
 */
const TIME_STEPS_MS = [
	1000,
	5000,
	10000,
	15000,
	30000,
	60000,
	2 * 60000,
	5 * 60000,
	10 * 60000,
	15 * 60000,
	30 * 60000,
	3600000,
	2 * 3600000,
	3 * 3600000,
	6 * 3600000,
	12 * 3600000
];

/** Room a label needs to itself, in pixels, before the next one may start. */
const TICK_SPACING_PX = 82;

/**
 * Times are written on a 24-hour clock whatever the browser's locale would
 * default to, so midnight reads 00:00 and the labels stay the same width.
 * `hourCycle` rather than `hour12: false`, which some locales answer with a
 * 24 o'clock.
 */
const CLOCK = { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' };

/**
 * The ticks of the axis, placed in the coordinates of the drawing.
 *
 * `left` is where the plot starts and `availableWidth` how wide it is, both in
 * pixels; the returned `x` is measured in the same units. `anchor` is how the
 * label should be aligned: one centred on a tick near either end would hang
 * over the edge of the plot, so the outermost ones are aligned inwards.
 */
export function timeAxisOf(from, to, left, availableWidth) {
	const right = left + availableWidth;
	const span = to - from;

	if (!(span > 0)) {
		return [];
	}

	return timeTicks(from, to, availableWidth).map((tick) => {
		const x = left + ((tick.t - from) / span) * availableWidth;
		const anchor = x - left < 28 ? 'start' : right - x < 28 ? 'end' : 'middle';

		return { t: tick.t, x, label: tick.label, anchor };
	});
}

/** Round moments inside the range, spaced far enough apart to be legible. */
export function timeTicks(from, to, availableWidth) {
	const span = to - from;
	if (!(span > 0) || availableWidth <= 0) {
		return [];
	}

	const mostThatFit = Math.max(2, Math.floor(availableWidth / TICK_SPACING_PX));
	const smallestStep = span / mostThatFit;
	const step = TIME_STEPS_MS.find((candidate) => candidate >= smallestStep);

	const times =
		step === undefined ? midnights(from, to, smallestStep) : roundMoments(from, to, step);

	// Over a range that crosses midnight the time of day alone is ambiguous,
	// so the tick that starts each day carries the date.
	const spansDays = startOfDay(from) !== startOfDay(to);

	return times.map((t) => ({ t, label: tickLabel(t, step ?? DAY_MS, spansDays) }));
}

/**
 * Whole multiples of `step`, counted from the local midnight before the range
 * starts rather than from the Unix epoch, so the labels line up with the clock
 * even where the time zone is not a whole number of hours.
 */
function roundMoments(from, to, step) {
	const origin = startOfDay(from);
	const times = [];

	for (let t = origin + Math.ceil((from - origin) / step) * step; t <= to; t += step) {
		times.push(t);
	}

	return times;
}

/** Local midnights, every `days` of them, walked by date rather than by sum. */
function midnights(from, to, smallestStep) {
	const days =
		[1, 2, 7, 14, 28].find((candidate) => candidate * DAY_MS >= smallestStep) ??
		Math.ceil(smallestStep / DAY_MS);

	const times = [];
	const cursor = new Date(startOfDay(from));
	if (cursor.getTime() < from) {
		cursor.setDate(cursor.getDate() + 1);
	}

	while (cursor.getTime() <= to) {
		times.push(cursor.getTime());
		cursor.setDate(cursor.getDate() + days);
	}

	return times;
}

/** The local midnight at or before `t`. */
export function startOfDay(t) {
	const date = new Date(t);
	date.setHours(0, 0, 0, 0);
	return date.getTime();
}

/** As much of the moment as the step makes meaningful, and no more. */
function tickLabel(t, step, spansDays) {
	const date = new Date(t);
	const atMidnight = date.getHours() === 0 && date.getMinutes() === 0 && date.getSeconds() === 0;

	if (step >= DAY_MS || (spansDays && atMidnight)) {
		return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
	}

	if (step < 60000) {
		return date.toLocaleTimeString(undefined, { ...CLOCK, second: '2-digit' });
	}

	return date.toLocaleTimeString(undefined, CLOCK);
}

/**
 * One moment written out, to the precision a range of `span` milliseconds
 * justifies: the date is only worth printing when the range covers several
 * days, and the seconds only when it is short enough for them to differ.
 */
export function formatTime(t, span) {
	const date = new Date(t);

	if (span > 2 * DAY_MS) {
		return date.toLocaleString(undefined, { month: 'short', day: 'numeric', ...CLOCK });
	}

	if (span > 30 * 60000) {
		return date.toLocaleTimeString(undefined, CLOCK);
	}

	return date.toLocaleTimeString(undefined, { ...CLOCK, second: '2-digit' });
}
