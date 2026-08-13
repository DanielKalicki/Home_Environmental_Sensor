<script>
	/**
	 * The AS7343's newest spectral reading, as one bar per filtered channel.
	 *
	 * The bars are the raw counts the sensor's converter returned. They are not
	 * corrected for how differently the channels respond and are not divided by
	 * the gain, so one bar standing above its neighbour does not by itself mean
	 * there is more light at that wavelength; what a bar shows reliably is how
	 * that one channel changes. The bars are tinted with the approximate colour
	 * of their wavelength, which is a label, not a measurement.
	 */
	import { SPECTRAL_WAVELENGTHS_NM } from '$lib/sensors.js';

	/** The newest AS7343 reading, or `null` if none has been collected. */
	export let reading = null;

	/** Drawn in real pixels, like the line charts, so the labels stay legible. */
	const HEIGHT = 230;
	const PADDING = { top: 14, right: 14, bottom: 36, left: 60 };

	const PLOT_HEIGHT = HEIGHT - PADDING.top - PADDING.bottom;

	/** Measured width of the chart, in pixels; the fallback is for the server. */
	let width = 640;

	$: plotWidth = Math.max(1, width - PADDING.left - PADDING.right);

	$: channels = SPECTRAL_WAVELENGTHS_NM.map((nm) => ({
		nm,
		counts: reading?.[`nm_${nm}`] ?? null
	})).filter((channel) => typeof channel.counts === 'number');

	$: maxCounts = channels.length > 0 ? Math.max(...channels.map((c) => c.counts), 1) : 1;
	$: slotWidth = channels.length > 0 ? plotWidth / channels.length : 0;
	$: barWidth = slotWidth * 0.7;
	$: bars = channels.map((channel, index) => ({
		...channel,
		x: PADDING.left + slotWidth * (index + 0.15),
		height: barHeight(channel.counts)
	}));

	function barHeight(counts) {
		return (counts / maxCounts) * PLOT_HEIGHT;
	}

	/**
	 * Roughly the colour a wavelength appears as, for labelling the bars.
	 *
	 * 855 nm is infrared and 405 nm is at the edge of what the eye responds to;
	 * both are given the nearest visible hue rather than a colour of their own.
	 */
	function colorOf(nm) {
		if (nm >= 780) return '#7f1d1d';
		if (nm >= 645) return '#ef4444';
		if (nm >= 590) return '#f97316';
		if (nm >= 565) return '#eab308';
		if (nm >= 520) return '#22c55e';
		if (nm >= 490) return '#06b6d4';
		if (nm >= 440) return '#3b82f6';
		return '#8b5cf6';
	}

	function formatCounts(counts) {
		return counts.toLocaleString(undefined, { maximumFractionDigits: 0 });
	}
</script>

<figure class="chart">
	<figcaption>
		<h3>Spectrum (raw counts per channel)</h3>
		<span class="meta">
			{#if reading}
				gain ×{reading.gain ?? '?'}
				{#if reading.analog_saturation || reading.digital_saturation}
					<em class="saturated">saturated — counts are cut off</em>
				{/if}
			{/if}
		</span>
	</figcaption>

	<div class="plot" bind:clientWidth={width}>
		<svg
			viewBox="0 0 {width} {HEIGHT}"
			width={width}
			height={HEIGHT}
			role="img"
			aria-label="Spectral channels"
		>
			{#if bars.length > 0}
				<line
					class="axis-line"
					x1={PADDING.left}
					x2={width - PADDING.right}
					y1={PADDING.top + PLOT_HEIGHT}
					y2={PADDING.top + PLOT_HEIGHT}
				/>
				<text class="axis" x={PADDING.left - 8} y={PADDING.top + 10} text-anchor="end">
					{formatCounts(maxCounts)}
				</text>
				<text class="axis" x={PADDING.left - 8} y={PADDING.top + PLOT_HEIGHT} text-anchor="end"
					>0</text
				>

				{#each bars as bar}
					<rect
						x={bar.x}
						y={PADDING.top + PLOT_HEIGHT - bar.height}
						width={barWidth}
						height={bar.height}
						fill={colorOf(bar.nm)}
						rx="2"
					>
						<title>{bar.nm} nm: {formatCounts(bar.counts)} counts</title>
					</rect>
					<text class="axis" x={bar.x + barWidth / 2} y={HEIGHT - 20} text-anchor="middle">
						{bar.nm}
					</text>
				{/each}
				<text class="axis" x={width / 2} y={HEIGHT - 5} text-anchor="middle">wavelength (nm)</text>
			{:else}
				<text class="empty" x={width / 2} y={HEIGHT / 2} text-anchor="middle">
					no spectral reading collected yet
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

	/* Stacked to match the line charts, so every card in a row puts its title
	   and its labels at the same height. */
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

	.meta {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.2rem 1rem;
		min-height: 1.2rem;
		font-size: 0.8rem;
		color: #94a3b8;
	}

	.saturated {
		color: #f97316;
		font-style: normal;
	}

	.plot {
		width: 100%;
	}

	svg {
		display: block;
		width: 100%;
	}

	.axis-line {
		stroke: #1f2937;
		stroke-width: 1;
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
