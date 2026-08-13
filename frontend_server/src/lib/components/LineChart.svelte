<script>
	/**
	 * A line chart drawn as plain SVG.
	 *
	 * Every series is a list of `{ t, v }` points, `t` in milliseconds since the
	 * Unix epoch. The horizontal axis is fixed to `from`..`to` rather than to
	 * the data, so a chart whose sensor has stopped reporting shows the gap
	 * instead of stretching its last readings across the full width. Points are
	 * joined only when they are close enough together to be consecutive
	 * readings; a longer gap breaks the line, so an interruption in the history
	 * is visible as one.
	 */
	export let title = '';
	export let unit = '';
	export let decimals = 0;
	export let series = [];
	export let from = 0;
	export let to = 1;

	/**
	 * The chart is drawn in real pixels, not in a fixed viewBox that the browser
	 * then scales to fit. A scaled viewBox shrinks the axis labels along with the
	 * chart, so as soon as several charts share the width of the window the
	 * numbers become unreadable. The width is measured from the element itself,
	 * which makes one user unit exactly one pixel: the sizes written below are
	 * the sizes the labels are actually drawn at.
	 */
	const HEIGHT = 230;
	const PADDING = { top: 14, right: 14, bottom: 30, left: 60 };

	const PLOT_HEIGHT = HEIGHT - PADDING.top - PADDING.bottom;

	/** Measured width of the chart, in pixels; the fallback is for the server. */
	let width = 640;

	$: plotWidth = Math.max(1, width - PADDING.left - PADDING.right);

	/**
	 * Longest gap between two points that is still drawn as a connected line,
	 * as a multiple of the median gap of the series. Anything longer is a break
	 * in the record rather than a slow-changing measurement.
	 */
	const GAP_FACTOR = 4;

	const DAY_MS = 86400000;

	/**
	 * The steps the horizontal axis is allowed to use, in milliseconds. A label
	 * therefore always falls on a whole minute, quarter hour, hour and so on,
	 * rather than on whatever moment the range happens to begin at. Anything
	 * coarser than the last entry is counted in whole days instead, because days
	 * are not all the same length once the clocks change.
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

	let hoverX = null;

	$: allValues = series.flatMap((entry) => entry.points.map((point) => point.v));
	$: hasData = allValues.length > 0;
	$: bounds = computeBounds(allValues);
	$: ticks = hasData ? tickValues(bounds.min, bounds.max) : [];
	$: timeAxis = hasData ? timeAxisOf(from, to, plotWidth) : [];
	$: hoverTime =
		hoverX === null ? null : from + ((hoverX - PADDING.left) / plotWidth) * (to - from);
	$: hovered = hoverTime === null ? [] : series.map((entry) => nearest(entry, hoverTime));

	// The drawn shapes are derived here, naming every value they are built from,
	// because Svelte decides what a statement depends on from the names it
	// mentions. Calling these out of the markup instead would leave a path drawn
	// against a scale that has since changed.
	$: paths = series.map((entry) => ({
		color: entry.color,
		d: pathOf(entry.points, bounds, from, to, plotWidth)
	}));
	$: markers = hovered.map((point) =>
		point ? { cx: scaleX(point.t, from, to, plotWidth), cy: scaleY(point.v, bounds) } : null
	);

	function computeBounds(values) {
		if (values.length === 0) {
			return { min: 0, max: 1 };
		}

		let min = Math.min(...values);
		let max = Math.max(...values);

		if (min === max) {
			// A flat series would otherwise have zero height to scale into.
			const margin = Math.abs(min) > 0 ? Math.abs(min) * 0.05 : 1;
			min -= margin;
			max += margin;
		} else {
			const margin = (max - min) * 0.08;
			min -= margin;
			max += margin;
		}

		return { min, max };
	}

	/** Horizontal position of a time, within the range the chart covers. */
	function scaleX(t, rangeFrom, rangeTo, availableWidth) {
		return PADDING.left + ((t - rangeFrom) / (rangeTo - rangeFrom)) * availableWidth;
	}

	/** Vertical position of a value, within the bounds the chart is scaled to. */
	function scaleY(v, valueBounds) {
		return (
			PADDING.top +
			(1 - (v - valueBounds.min) / (valueBounds.max - valueBounds.min)) * PLOT_HEIGHT
		);
	}

	/** The series as an SVG path, with a break wherever readings are missing. */
	function pathOf(points, valueBounds, rangeFrom, rangeTo, availableWidth) {
		if (points.length === 0) {
			return '';
		}

		const gaps = [];
		for (let i = 1; i < points.length; i += 1) {
			gaps.push(points[i].t - points[i - 1].t);
		}
		gaps.sort((a, b) => a - b);
		const typicalGap = gaps.length > 0 ? gaps[Math.floor(gaps.length / 2)] : 0;
		const maxGap = typicalGap > 0 ? typicalGap * GAP_FACTOR : Infinity;

		let path = '';
		let penDown = false;

		for (let i = 0; i < points.length; i += 1) {
			const point = points[i];
			const broken = i > 0 && point.t - points[i - 1].t > maxGap;
			const x = scaleX(point.t, rangeFrom, rangeTo, availableWidth).toFixed(2);
			const y = scaleY(point.v, valueBounds).toFixed(2);

			if (!penDown || broken) {
				path += `M${x} ${y}`;
				penDown = true;
			} else {
				path += `L${x} ${y}`;
			}
		}

		return path;
	}

	/** Five round-ish values spanning the vertical axis. */
	function tickValues(min, max) {
		const count = 5;
		const step = (max - min) / (count - 1);
		return Array.from({ length: count }, (_, i) => min + step * i);
	}

	/** The horizontal axis: where each label sits, and how it is anchored. */
	function timeAxisOf(rangeFrom, rangeTo, availableWidth) {
		const left = PADDING.left;
		const right = left + availableWidth;

		return timeTicks(rangeFrom, rangeTo, availableWidth).map((tick) => {
			const x = scaleX(tick.t, rangeFrom, rangeTo, availableWidth);

			// A label centred on a tick near either end would hang over the edge
			// of the plot, so the outermost ones are aligned inwards instead.
			const anchor = x - left < 28 ? 'start' : right - x < 28 ? 'end' : 'middle';

			return { x, label: tick.label, anchor };
		});
	}

	/** Round moments inside the range, spaced far enough apart to be legible. */
	function timeTicks(rangeFrom, rangeTo, availableWidth) {
		const span = rangeTo - rangeFrom;
		if (!(span > 0) || availableWidth <= 0) {
			return [];
		}

		const mostThatFit = Math.max(2, Math.floor(availableWidth / TICK_SPACING_PX));
		const smallestStep = span / mostThatFit;
		const step = TIME_STEPS_MS.find((candidate) => candidate >= smallestStep);

		const times =
			step === undefined
				? midnights(rangeFrom, rangeTo, smallestStep)
				: roundMoments(rangeFrom, rangeTo, step);

		// Over a range that crosses midnight the time of day alone is ambiguous,
		// so the tick that starts each day carries the date.
		const spansDays = startOfDay(rangeFrom) !== startOfDay(rangeTo);

		return times.map((t) => ({ t, label: tickLabel(t, step ?? DAY_MS, spansDays) }));
	}

	/**
	 * Whole multiples of `step`, counted from the local midnight before the
	 * range starts rather than from the Unix epoch, so the labels line up with
	 * the clock even where the time zone is not a whole number of hours.
	 */
	function roundMoments(rangeFrom, rangeTo, step) {
		const origin = startOfDay(rangeFrom);
		const times = [];

		for (let t = origin + Math.ceil((rangeFrom - origin) / step) * step; t <= rangeTo; t += step) {
			times.push(t);
		}

		return times;
	}

	/** Local midnights, every `days` of them, walked by date rather than by sum. */
	function midnights(rangeFrom, rangeTo, smallestStep) {
		const days =
			[1, 2, 7, 14, 28].find((candidate) => candidate * DAY_MS >= smallestStep) ??
			Math.ceil(smallestStep / DAY_MS);

		const times = [];
		const cursor = new Date(startOfDay(rangeFrom));
		if (cursor.getTime() < rangeFrom) {
			cursor.setDate(cursor.getDate() + 1);
		}

		while (cursor.getTime() <= rangeTo) {
			times.push(cursor.getTime());
			cursor.setDate(cursor.getDate() + days);
		}

		return times;
	}

	function startOfDay(t) {
		const date = new Date(t);
		date.setHours(0, 0, 0, 0);
		return date.getTime();
	}

	/** As much of the moment as the step makes meaningful, and no more. */
	function tickLabel(t, step, spansDays) {
		const date = new Date(t);
		const atMidnight =
			date.getHours() === 0 && date.getMinutes() === 0 && date.getSeconds() === 0;

		if (step >= DAY_MS || (spansDays && atMidnight)) {
			return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
		}

		if (step < 60000) {
			return date.toLocaleTimeString(undefined, {
				hour: '2-digit',
				minute: '2-digit',
				second: '2-digit'
			});
		}

		return date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
	}

	function formatValue(value) {
		if (value === null || value === undefined) {
			return '—';
		}
		if (Math.abs(value) >= 100000) {
			return value.toExponential(2);
		}
		return value.toLocaleString(undefined, {
			minimumFractionDigits: decimals,
			maximumFractionDigits: decimals
		});
	}

	/** The moment under the cursor, to the precision the range justifies. */
	function formatTime(t) {
		const date = new Date(t);
		const span = to - from;

		if (span > 2 * DAY_MS) {
			return date.toLocaleString(undefined, {
				month: 'short',
				day: 'numeric',
				hour: '2-digit',
				minute: '2-digit'
			});
		}

		if (span > 30 * 60000) {
			return date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
		}

		return date.toLocaleTimeString(undefined, {
			hour: '2-digit',
			minute: '2-digit',
			second: '2-digit'
		});
	}

	/** The point of `entry` closest in time to `t`, or `null` if far from any. */
	function nearest(entry, t) {
		let best = null;
		let bestDistance = Infinity;

		for (const point of entry.points) {
			const distance = Math.abs(point.t - t);
			if (distance < bestDistance) {
				bestDistance = distance;
				best = point;
			}
		}

		// Ignore a match further away than a fiftieth of the visible range: the
		// cursor is then over a stretch where the series has nothing to show.
		return bestDistance <= (to - from) / 50 ? best : null;
	}

	function onPointerMove(event) {
		const rect = event.currentTarget.getBoundingClientRect();
		hoverX = event.clientX - rect.left;
	}
</script>

<figure class="chart">
	<figcaption>
		<h3>{title}{unit ? ` (${unit})` : ''}</h3>
		<ul class="legend">
			{#each series as entry, index}
				<li>
					<span class="swatch" style:background={entry.color}></span>
					{entry.label}
					<strong>{formatValue(hovered[index]?.v ?? entry.points[entry.points.length - 1]?.v)}</strong>
				</li>
			{/each}
		</ul>
	</figcaption>

	<div class="plot" bind:clientWidth={width}>
		<svg
			viewBox="0 0 {width} {HEIGHT}"
			width={width}
			height={HEIGHT}
			role="img"
			aria-label={title}
			on:pointermove={onPointerMove}
			on:pointerleave={() => (hoverX = null)}
		>
			{#if hasData}
				{#each ticks as tick}
					<line
						class="grid"
						x1={PADDING.left}
						x2={width - PADDING.right}
						y1={scaleY(tick, bounds)}
						y2={scaleY(tick, bounds)}
					/>
					<text class="axis" x={PADDING.left - 8} y={scaleY(tick, bounds) + 4} text-anchor="end">
						{formatValue(tick)}
					</text>
				{/each}

				{#each timeAxis as tick}
					<line
						class="grid"
						x1={tick.x}
						x2={tick.x}
						y1={PADDING.top}
						y2={PADDING.top + PLOT_HEIGHT}
					/>
				{/each}

				{#each paths as line}
					<path d={line.d} fill="none" stroke={line.color} stroke-width="2" />
				{/each}

				<line
					class="axis-line"
					x1={PADDING.left}
					x2={width - PADDING.right}
					y1={PADDING.top + PLOT_HEIGHT}
					y2={PADDING.top + PLOT_HEIGHT}
				/>

				{#each timeAxis as tick}
					<line
						class="axis-line"
						x1={tick.x}
						x2={tick.x}
						y1={PADDING.top + PLOT_HEIGHT}
						y2={PADDING.top + PLOT_HEIGHT + 4}
					/>
					<text class="axis" x={tick.x} y={HEIGHT - 8} text-anchor={tick.anchor}>
						{tick.label}
					</text>
				{/each}

				{#if hoverX !== null && hoverX >= PADDING.left && hoverX <= width - PADDING.right}
					<line
						class="cursor"
						x1={hoverX}
						x2={hoverX}
						y1={PADDING.top}
						y2={HEIGHT - PADDING.bottom}
					/>
					{#each markers as marker}
						{#if marker}
							<circle cx={marker.cx} cy={marker.cy} r="4" fill="#0f172a" stroke="#e2e8f0" />
						{/if}
					{/each}
					<text class="axis" x={width / 2} y={PADDING.top + 12} text-anchor="middle">
						{formatTime(hoverTime)}
					</text>
				{/if}
			{:else}
				<text class="empty" x={width / 2} y={HEIGHT / 2} text-anchor="middle">
					no readings in this range
				</text>
			{/if}
		</svg>
	</div>
</figure>

<style>
	.chart {
		margin: 0;
		padding: 0.6rem 0.75rem 0.4rem;
		background: #111827;
		border: 1px solid #1f2937;
		border-radius: 10px;
	}

	/*
	 * The title and the legend are stacked rather than laid out as a wrapping
	 * row. A row would put a short legend beside the title and drop a long one
	 * onto its own line, so the same page showed the labels in two different
	 * places depending on how many series a chart happens to have.
	 */
	figcaption {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 0.2rem;
		margin-bottom: 0.15rem;
	}

	h3 {
		margin: 0;
		font-size: 0.95rem;
		font-weight: 600;
		color: #e2e8f0;
	}

	.legend {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.2rem 1rem;
		/* Held open so a chart with one series is exactly as tall as one with
		   three, and the charts in a row keep a common baseline. */
		min-height: 1.2rem;
		margin: 0;
		padding: 0;
		list-style: none;
		font-size: 0.8rem;
		color: #94a3b8;
	}

	.legend li {
		display: flex;
		align-items: center;
		gap: 0.35rem;
	}

	.legend strong {
		color: #e2e8f0;
		font-variant-numeric: tabular-nums;
	}

	.swatch {
		width: 0.7rem;
		height: 0.7rem;
		border-radius: 2px;
	}

	.plot {
		width: 100%;
	}

	svg {
		display: block;
		width: 100%;
		touch-action: none;
	}

	.grid {
		stroke: #1f2937;
		stroke-width: 1;
	}

	.axis-line {
		stroke: #334155;
		stroke-width: 1;
	}

	.cursor {
		stroke: #475569;
		stroke-width: 1;
		stroke-dasharray: 3 3;
	}

	.axis {
		fill: #94a3b8;
		font-size: 12px;
		font-family: inherit;
	}

	.empty {
		fill: #475569;
		font-size: 14px;
		font-family: inherit;
	}
</style>
