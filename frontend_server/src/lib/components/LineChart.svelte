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
	import { createEventDispatcher } from 'svelte';
	import { formatTime, timeAxisOf } from '$lib/timeAxis.js';

	export let title = '';
	export let unit = '';
	export let decimals = 0;
	/**
	 * Each entry may carry a `sensor` (the id a legend click reports) and a
	 * `hidden` flag. A hidden series stays in the legend, dimmed, so clicking
	 * it again brings it back; it is left out of the drawn line and of the
	 * axis's range so a chart the reader has narrowed to two series is not
	 * still scaled for a third they turned off.
	 */
	export let series = [];
	export let from = 0;
	export let to = 1;

	const dispatch = createEventDispatcher();

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

	/** How many labels the vertical axis aims for; the rounding may vary it. */
	const VALUE_TICKS = 5;

	let hoverX = null;

	$: allValues = series
		.filter((entry) => !entry.hidden)
		.flatMap((entry) => entry.points.map((point) => point.v));
	$: hasData = allValues.length > 0;
	$: bounds = scaleOf(allValues);
	$: ticks = hasData ? bounds.ticks : [];
	$: timeAxis = hasData ? timeAxisOf(from, to, PADDING.left, plotWidth) : [];
	$: hoverTime =
		hoverX === null ? null : from + ((hoverX - PADDING.left) / plotWidth) * (to - from);
	$: hovered =
		hoverTime === null
			? []
			: series.map((entry) => (entry.hidden ? null : nearest(entry, hoverTime)));

	// The drawn shapes are derived here, naming every value they are built from,
	// because Svelte decides what a statement depends on from the names it
	// mentions. Calling these out of the markup instead would leave a path drawn
	// against a scale that has since changed. A hidden series gets no path at
	// all, rather than one drawn and then covered up, so it plays no part in
	// the picture beyond its dimmed entry in the legend.
	$: paths = series.map((entry) => ({
		color: entry.color,
		d: entry.hidden ? '' : pathOf(entry.points, bounds, from, to, plotWidth)
	}));

	/** Tells the page a legend entry was clicked, so it can flip that sensor. */
	function toggle(entry) {
		if (entry.sensor) {
			dispatch('toggle', entry.sensor);
		}
	}
	$: markers = hovered.map((point) =>
		point ? { cx: scaleX(point.t, from, to, plotWidth), cy: scaleY(point.v, bounds) } : null
	);

	/**
	 * The vertical scale: the bounds the plot is drawn against, the step between
	 * labels, and the labelled values themselves.
	 *
	 * Two things are deliberate here. The bounds are rounded outwards to whole
	 * multiples of a step people count in, so the axis reads 0, 25, 50 rather
	 * than -3.7, 8.4, 20.5. And the axis is never taken below zero unless the
	 * data itself goes there: the margin that keeps a line off the edge of the
	 * plot would otherwise invent readings that cannot exist, such as a negative
	 * air quality index or a negative concentration. A quantity that does go
	 * below zero, such as an outdoor temperature, still scales normally, because
	 * the rule looks at the readings rather than at the name of the chart.
	 */
	function scaleOf(values) {
		if (values.length === 0) {
			return { min: 0, max: 1, step: 1, ticks: [] };
		}

		const lowest = Math.min(...values);
		const highest = Math.max(...values);
		const goesNegative = lowest < 0;

		let min = lowest;
		let max = highest;

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

		const step = chooseStep(min, max);

		min = Math.floor(min / step) * step;
		max = Math.ceil(max / step) * step;

		if (!goesNegative && min < 0) {
			min = 0;
		}

		if (max <= min) {
			max = min + step;
		}

		const ticks = [];
		// The comparison is loosened by a thousandth of a step because the sum
		// below drifts: ten additions of 0.1 do not land exactly on 1.
		for (let value = min; value <= max + step / 1000; value += step) {
			ticks.push(exactly(value));
		}

		return { min: exactly(min), max: exactly(max), step, ticks };
	}

	/**
	 * The finest step that still labels the range in no more than the labels
	 * wanted, taken from the amounts people count in.
	 *
	 * Deriving the step from the span alone is not enough: rounding the bounds
	 * out to whole multiples of it can widen the axis a long way past the data,
	 * and a carbon dioxide range of 420 to 1350 would end up drawn against an
	 * axis of 0 to 1500 with the line squashed into the middle of it. Trying the
	 * candidates in order and stopping at the first that fits keeps the axis
	 * close to the readings.
	 */
	function chooseStep(min, max) {
		const span = max - min;
		if (!(span > 0) || !Number.isFinite(span)) {
			return 1;
		}

		const smallest = Math.floor(Math.log10(span)) - 3;

		for (let exponent = smallest; exponent <= smallest + 5; exponent += 1) {
			for (const factor of [1, 2, 2.5, 5]) {
				const step = factor * 10 ** exponent;
				if (Math.ceil(max / step) - Math.floor(min / step) + 1 <= VALUE_TICKS + 1) {
					return step;
				}
			}
		}

		return span / (VALUE_TICKS - 1);
	}

	/** Drops the drift that repeated addition leaves in a decimal step. */
	function exactly(value) {
		return Number(value.toPrecision(12));
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

	function formatValue(value, places = decimals) {
		if (value === null || value === undefined) {
			return '—';
		}
		if (Math.abs(value) >= 100000) {
			return value.toExponential(2);
		}
		return value.toLocaleString(undefined, {
			minimumFractionDigits: places,
			maximumFractionDigits: places
		});
	}

	/**
	 * A label on the vertical axis. It carries at least as many decimals as the
	 * step between ticks has, so an axis stepping by 0.5 does not print the same
	 * number twice because the chart itself is written in whole units.
	 */
	function formatTick(value, step) {
		return formatValue(value, Math.max(decimals, decimalsOf(step)));
	}

	function decimalsOf(step) {
		const text = String(exactly(step));
		if (text.includes('e')) {
			return 6;
		}

		const dot = text.indexOf('.');
		return dot === -1 ? 0 : Math.min(6, text.length - dot - 1);
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
				<li class:disabled={entry.hidden}>
					{#if entry.sensor}
						<button
							type="button"
							class="legend-item"
							title={entry.hidden ? 'Click to show on this chart' : 'Click to hide on this chart'}
							on:click={() => toggle(entry)}
						>
							<span class="swatch" style:background={entry.color}></span>
							{entry.label}
						</button>
					{:else}
						<span class="legend-item">
							<span class="swatch" style:background={entry.color}></span>
							{entry.label}
						</span>
					{/if}
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
						{formatTick(tick, bounds.step)}
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
					{#if line.d}
						<path d={line.d} fill="none" stroke={line.color} stroke-width="2" />
					{/if}
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
						{formatTime(hoverTime, to - from)}
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

	.legend-item {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		border: none;
		background: none;
		padding: 0;
		margin: 0;
		font: inherit;
		color: inherit;
	}

	/* Only series the page can actually toggle look interactive; a series
	   with no `sensor` to report has nothing for a click to do. */
	button.legend-item {
		cursor: pointer;
		border-radius: 4px;
		padding: 0.05rem 0.3rem;
		margin: -0.05rem -0.3rem;
	}

	button.legend-item:hover {
		background: #1f2937;
	}

	button.legend-item:focus-visible {
		outline: 2px solid #4f9cf9;
		outline-offset: 1px;
	}

	.legend li.disabled {
		opacity: 0.45;
		text-decoration: line-through;
	}

	.legend li.disabled .swatch {
		background: #475569 !important;
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
