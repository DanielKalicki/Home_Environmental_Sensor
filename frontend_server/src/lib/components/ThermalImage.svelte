<script>
	/**
	 * The last picture the MLX90640 thermal camera took.
	 *
	 * The camera is a 32 by 24 grid of thermometers, not a camera in the
	 * ordinary sense: each of its 768 pixels reports the temperature of
	 * whatever part of the scene it is pointed at, in degrees Celsius. That is
	 * the whole picture, so it is drawn 32 pixels wide and stretched to fill
	 * the card; the blockiness is the real resolution of the instrument.
	 *
	 * Colour is the measurement. The scale runs from the coldest pixel of this
	 * image to the warmest, and both ends are printed beside it, so the same
	 * colour means a different temperature in the next image: the picture shows
	 * what is warmer than what, and the numbers say by how much. It is drawn
	 * with matplotlib's inferno, which rises evenly in lightness, so equal
	 * steps of colour are equal steps of temperature.
	 *
	 * What the temperatures mean depends on the surface. The camera measures
	 * the infrared a surface emits and assumes every surface emits as well as
	 * painted wall, wood, cloth or skin does. Bare or polished metal emits far
	 * less than that and reflects its surroundings instead, so it reads far
	 * colder than it is; that is a property of the surface, not a fault. Glass
	 * is opaque to what the camera sees, so a window shows the temperature of
	 * the pane rather than of anything behind it.
	 *
	 * There is no history: the device keeps only the newest image, so this is
	 * always the last one taken and there is nothing to scrub back through.
	 */

	/**
	 * The image as `/api/thermal` returns it, or `null` before the first one
	 * has been collected.
	 */
	export let image = null;

	/** Local time now, in milliseconds, so the image's age can be shown. */
	export let now = Date.now();

	/**
	 * The colour scale: matplotlib's inferno, sampled. Read dark to bright as
	 * cold to hot.
	 */
	const INFERNO = [
		'#000004',
		'#1b0c41',
		'#4a0c6b',
		'#781c6d',
		'#a52c60',
		'#cf4446',
		'#ed6925',
		'#fb9b06',
		'#f7d13d',
		'#fcffa4'
	];

	/** The scale expanded once into fixed steps, rather than mixed per pixel. */
	const RAMP = rampOf(INFERNO, 256);

	/**
	 * Whether to blend between pixels when the image is stretched.
	 *
	 * Off by default: a blocky picture shows exactly how much the instrument
	 * actually resolved, and smoothing invents detail between the pixels that
	 * was never measured. It is offered because a smoothed picture is easier to
	 * recognise a scene in.
	 */
	let smooth = false;

	/** The visible, stretched picture. */
	let canvas = null;

	/** The picture at its real size, stretched onto `canvas` when it is drawn. */
	let source = null;

	/** Pixel the cursor is over, as a row and a column, or `null`. */
	let hover = null;

	$: pixels = Array.isArray(image?.pixels) ? image.pixels : [];
	$: width = image?.width ?? 32;
	$: height = image?.height ?? 24;
	$: available = image?.available === true && pixels.length === width * height;

	/**
	 * The ends of the colour scale. The device sends the extremes it worked out
	 * from the same pixels; they are recomputed here so the colours cannot
	 * disagree with the numbers printed beside them if the two ever differ.
	 */
	$: range = rangeOf(pixels);

	$: hoveredCelsius =
		hover === null || !available ? null : (pixels[hover.row * width + hover.column] ?? null);

	$: ageSeconds = image?.takenAt ? Math.max(0, Math.round((now - image.takenAt) / 1000)) : null;

	// Redraw whenever the picture, the scale or the smoothing changes.
	$: draw(canvas, pixels, width, height, range, smooth, available);

	/** Paint the temperatures, then stretch them over the card. */
	function draw(target, values, columns, rows, scale, interpolate, ready) {
		if (!target || !ready) {
			return;
		}

		const context = target.getContext('2d');
		if (!context) {
			return;
		}

		// The temperatures are painted once at their real size, into a canvas
		// of exactly 32 by 24, and that canvas is then stretched. Drawing 768
		// rectangles onto the large canvas instead would be the same picture,
		// but this way the browser does the stretching and can blend between
		// the pixels when asked to.
		if (!source) {
			source = document.createElement('canvas');
		}
		if (source.width !== columns || source.height !== rows) {
			source.width = columns;
			source.height = rows;
		}

		const sourceContext = source.getContext('2d');
		if (!sourceContext) {
			return;
		}

		const picture = sourceContext.createImageData(columns, rows);
		const span = scale.max - scale.min;

		for (let index = 0; index < columns * rows; index += 1) {
			const value = values[index];
			const fraction = span > 0 && Number.isFinite(value) ? (value - scale.min) / span : 0;
			const colour = RAMP[Math.round(Math.min(1, Math.max(0, fraction)) * (RAMP.length - 1))];

			picture.data[index * 4] = colour[0];
			picture.data[index * 4 + 1] = colour[1];
			picture.data[index * 4 + 2] = colour[2];
			picture.data[index * 4 + 3] = 255;
		}

		sourceContext.putImageData(picture, 0, 0);

		// The card's width is not known until it is laid out, so the drawing
		// surface is sized from the element itself. Without this the canvas
		// keeps its default 300 by 150 and the picture comes out squashed.
		const ratio = typeof window === 'undefined' ? 1 : window.devicePixelRatio || 1;
		const boxWidth = Math.max(1, Math.round(target.clientWidth * ratio));
		const boxHeight = Math.max(1, Math.round((boxWidth * rows) / columns));

		if (target.width !== boxWidth || target.height !== boxHeight) {
			target.width = boxWidth;
			target.height = boxHeight;
		}

		context.imageSmoothingEnabled = interpolate;
		context.clearRect(0, 0, target.width, target.height);
		context.drawImage(source, 0, 0, target.width, target.height);
	}

	/** The coldest and warmest pixel of an image. */
	function rangeOf(values) {
		let min = Infinity;
		let max = -Infinity;

		for (const value of values) {
			if (!Number.isFinite(value)) {
				continue;
			}
			if (value < min) min = value;
			if (value > max) max = value;
		}

		return Number.isFinite(min) ? { min, max } : { min: 0, max: 0 };
	}

	/** The colour scale expanded into `steps` fixed colours, as RGB triples. */
	function rampOf(stops, steps) {
		const parsed = stops.map((hex) => [
			parseInt(hex.slice(1, 3), 16),
			parseInt(hex.slice(3, 5), 16),
			parseInt(hex.slice(5, 7), 16)
		]);

		const ramp = [];
		for (let step = 0; step < steps; step += 1) {
			const position = (step / (steps - 1)) * (parsed.length - 1);
			const lower = Math.min(parsed.length - 2, Math.floor(position));
			const t = position - lower;
			const from = parsed[lower];
			const to = parsed[lower + 1];

			ramp.push([
				Math.round(from[0] + (to[0] - from[0]) * t),
				Math.round(from[1] + (to[1] - from[1]) * t),
				Math.round(from[2] + (to[2] - from[2]) * t)
			]);
		}

		return ramp;
	}

	/** Which pixel of the picture the cursor is over. */
	function track(event) {
		if (!available) {
			return;
		}

		const box = event.currentTarget.getBoundingClientRect();
		const column = Math.floor(((event.clientX - box.left) / box.width) * width);
		const row = Math.floor(((event.clientY - box.top) / box.height) * height);

		hover =
			column >= 0 && column < width && row >= 0 && row < height ? { row, column } : null;
	}

	/** One decimal, which is about what the camera resolves. */
	function formatCelsius(value) {
		return Number.isFinite(value) ? `${value.toFixed(1)} °C` : '—';
	}
</script>

<svelte:window on:resize={() => draw(canvas, pixels, width, height, range, smooth, available)} />

<figure class="chart">
	<figcaption>
		<div class="heading">
			<h3>Thermal camera <span class="unit">(°C)</span></h3>

			<span class="meta">
				{#if available}
					<span>{width}×{height} pixels</span>
					<span>mean {formatCelsius(image.meanCelsius)}</span>
					<span title="The camera's own die, which the pixels are compensated against">
						sensor {formatCelsius(image.ambientCelsius)}
					</span>
					{#if ageSeconds !== null}
						<span>taken {ageSeconds} s ago</span>
					{/if}
				{/if}
			</span>
		</div>

		<div class="controls">
			<label class="check" title="Blend between the pixels instead of drawing them as blocks">
				<input type="checkbox" bind:checked={smooth} />
				smooth
			</label>

			<div class="scale-key" title="What the colours mean, for this image only">
				<span>{formatCelsius(range.min)}</span>
				<svg class="ramp" width="110" height="10" aria-hidden="true">
					<defs>
						<linearGradient id="thermal-inferno" x1="0" y1="0" x2="1" y2="0">
							{#each INFERNO as stop, index}
								<stop offset={index / (INFERNO.length - 1)} stop-color={stop} />
							{/each}
						</linearGradient>
					</defs>
					<rect width="110" height="10" fill="url(#thermal-inferno)" />
				</svg>
				<span>{formatCelsius(range.max)}</span>
			</div>
		</div>
	</figcaption>

	<div class="frame">
		<canvas
			bind:this={canvas}
			style="aspect-ratio: {width} / {height}"
			on:mousemove={track}
			on:mouseleave={() => (hover = null)}
		></canvas>

		{#if !available}
			<p class="empty">
				{#if image?.error}
					No image: {image.error}
				{:else}
					Waiting for the first image.
				{/if}
			</p>
		{/if}
	</div>

	<p class="readout">
		{#if hoveredCelsius !== null}
			row {hover.row + 1}, column {hover.column + 1}: <strong>{formatCelsius(hoveredCelsius)}</strong>
		{:else if available}
			coldest {formatCelsius(range.min)}, warmest {formatCelsius(range.max)} — point at the
			picture for a single pixel
		{:else}
			&nbsp;
		{/if}
	</p>
</figure>

<style>
	.chart {
		margin: 0;
		padding: 0.6rem 0.75rem 0.4rem;
		background: #111827;
		border: 1px solid #1f2937;
		border-radius: 10px;
	}

	figcaption {
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 0.35rem;
		margin-bottom: 0.35rem;
	}

	.heading {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		justify-content: space-between;
		gap: 0.2rem 1rem;
	}

	h3 {
		margin: 0;
		font-size: 0.95rem;
		font-weight: 600;
		color: #e2e8f0;
	}

	.unit {
		color: #94a3b8;
		font-weight: 400;
	}

	.meta {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.2rem 1rem;
		min-height: 1.2rem;
		font-size: 0.8rem;
		color: #94a3b8;
		font-variant-numeric: tabular-nums;
	}

	.controls {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.4rem 0.9rem;
	}

	.check {
		display: inline-flex;
		align-items: center;
		gap: 0.3rem;
		font-size: 0.75rem;
		color: #94a3b8;
		cursor: pointer;
	}

	.check input {
		margin: 0;
		accent-color: #4f9cf9;
	}

	.scale-key {
		display: inline-flex;
		align-items: center;
		gap: 0.35rem;
		margin-left: auto;
		font-size: 0.72rem;
		color: #94a3b8;
		font-variant-numeric: tabular-nums;
	}

	.ramp {
		display: block;
		border-radius: 2px;
	}

	.frame {
		position: relative;
	}

	canvas {
		display: block;
		width: 100%;
		border-radius: 6px;
		background: #0f172a;
	}

	.empty {
		position: absolute;
		inset: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		margin: 0;
		padding: 0 1rem;
		text-align: center;
		font-size: 0.85rem;
		color: #94a3b8;
	}

	.readout {
		margin: 0.35rem 0 0.2rem;
		min-height: 1.2rem;
		font-size: 0.78rem;
		color: #94a3b8;
		font-variant-numeric: tabular-nums;
	}

	.readout strong {
		color: #e2e8f0;
		font-weight: 600;
	}
</style>
