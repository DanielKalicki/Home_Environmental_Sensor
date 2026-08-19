<script>
	/**
	 * The OPT4048's colour, on the CIE 1931 chromaticity diagram.
	 *
	 * The sensor measures light through four filters shaped to the human eye's
	 * response, which gives a tristimulus value: the whole of what a colour is
	 * to a person looking at it. Dividing the brightness out of that leaves the
	 * pair of coordinates `x` and `y`, and every colour a person can see falls
	 * somewhere inside the horseshoe those coordinates trace out. That shape is
	 * what this chart draws, filled in with the approximate appearance of each
	 * point in it, with the readings plotted on top: one dot per reading, the
	 * older ones fainter, joined in the order they were taken so a light being
	 * switched on or the sun going down is a visible walk across the diagram.
	 *
	 * The curved edge of the horseshoe is the pure single wavelengths, labelled
	 * in nanometres; the straight edge closing it at the bottom is the line of
	 * purples, which no single wavelength produces. The curve through the
	 * middle is the black-body locus, where anything glowing because it is hot
	 * sits: a candle at one end, an overcast sky at the other. Ordinary indoor
	 * and outdoor light lands on or very near it, which is what makes a single
	 * colour temperature a fair description of it, and it is also why the
	 * readings crowd into one small part of the diagram. `Zoom to readings`
	 * rescales the axes to the measurements, which is the only way to see how
	 * the colour of a room actually moved.
	 *
	 * What the fill is and is not: the colours shown are what each point would
	 * look like on this screen at full brightness, so the diagram can be read
	 * as a map. Most of the horseshoe is outside what a screen can produce and
	 * is drawn desaturated towards white instead, so the saturated edges are
	 * far less vivid here than in life. The fill is a legend for position; it
	 * is not a measurement. The measurement is where the dots are.
	 */
	import { formatTime } from '$lib/timeAxis.js';

	/**
	 * The OPT4048's readings over the drawn range, oldest first, as the history
	 * endpoint returns them. A long range arrives averaged into buckets.
	 */
	export let readings = [];

	/**
	 * The newest reading, which the status endpoint refreshes more often than
	 * the history is fetched. It is drawn as one more point when it is newer
	 * than everything in `readings`, so the marker is current.
	 */
	export let latest = null;

	/** The range the chart covers, in milliseconds since the Unix epoch. */
	export let from = 0;
	export let to = 1;

	/**
	 * The spectral locus: the chromaticity of each pure wavelength, from the
	 * CIE 1931 2° standard observer, at 5 nm. Closing this list back on itself
	 * with a straight line gives the boundary of every visible colour.
	 */
	const LOCUS_NM = [
		[380, 0.1741, 0.005], [385, 0.174, 0.005], [390, 0.1738, 0.0049],
		[395, 0.1736, 0.0049], [400, 0.1733, 0.0048], [405, 0.173, 0.0048],
		[410, 0.1726, 0.0048], [415, 0.1721, 0.0048], [420, 0.1714, 0.0051],
		[425, 0.1703, 0.0058], [430, 0.1689, 0.0069], [435, 0.1669, 0.0086],
		[440, 0.1644, 0.0109], [445, 0.1611, 0.0138], [450, 0.1566, 0.0177],
		[455, 0.151, 0.0227], [460, 0.144, 0.0297], [465, 0.1355, 0.0399],
		[470, 0.1241, 0.0578], [475, 0.1096, 0.0868], [480, 0.0913, 0.1327],
		[485, 0.0687, 0.2007], [490, 0.0454, 0.295], [495, 0.0235, 0.4127],
		[500, 0.0082, 0.5384], [505, 0.0039, 0.6548], [510, 0.0139, 0.7502],
		[515, 0.0389, 0.812], [520, 0.0743, 0.8338], [525, 0.1142, 0.8262],
		[530, 0.1547, 0.8059], [535, 0.1929, 0.7816], [540, 0.2296, 0.7543],
		[545, 0.2658, 0.7243], [550, 0.3016, 0.6923], [555, 0.3373, 0.6589],
		[560, 0.3731, 0.6245], [565, 0.4087, 0.5896], [570, 0.4441, 0.5547],
		[575, 0.4788, 0.5202], [580, 0.5125, 0.4866], [585, 0.5448, 0.4544],
		[590, 0.5752, 0.4242], [595, 0.6029, 0.3965], [600, 0.627, 0.3725],
		[605, 0.6482, 0.3514], [610, 0.6658, 0.334], [615, 0.6801, 0.3197],
		[620, 0.6915, 0.3083], [625, 0.7006, 0.2993], [630, 0.7079, 0.292],
		[635, 0.714, 0.2859], [640, 0.719, 0.2809], [645, 0.723, 0.277],
		[650, 0.726, 0.274], [655, 0.7283, 0.2717], [660, 0.73, 0.27],
		[665, 0.7311, 0.2689], [670, 0.732, 0.268], [675, 0.7327, 0.2673],
		[680, 0.7334, 0.2666], [685, 0.734, 0.266], [690, 0.7344, 0.2656],
		[695, 0.7346, 0.2654], [700, 0.7347, 0.2653]
	];

	/** The same boundary as plain coordinate pairs, for the geometry below. */
	const LOCUS = LOCUS_NM.map(([, x, y]) => ({ x, y }));

	/** Wavelengths labelled along the curved edge, where there is room. */
	const LABELLED_NM = [460, 480, 490, 500, 510, 520, 540, 560, 580, 600, 620, 660];

	/** Colour temperatures marked along the black-body locus, in kelvin. */
	const ISOTHERMS = [2000, 2700, 3000, 4000, 5000, 6500, 10000];

	/**
	 * Standard illuminants drawn for reference. A is a tungsten filament lamp
	 * and D65 is average daylight, which between them bracket almost every
	 * light a room is lit by.
	 */
	const REFERENCES = [
		{ label: 'A', title: 'Illuminant A — tungsten filament, 2856 K', x: 0.4476, y: 0.4074 },
		{ label: 'D65', title: 'Illuminant D65 — average daylight, 6504 K', x: 0.3127, y: 0.329 }
	];

	/** The whole diagram, with a margin past the boundary on every side. */
	const FULL_DOMAIN = { x0: -0.02, x1: 0.78, y0: -0.02, y1: 0.88 };

	/** Drawn in real pixels, like the other charts; the fallback is the server's. */
	const PADDING = { top: 14, right: 16, bottom: 34, left: 46 };
	const MIN_SIZE = 260;

	/** Measured width of the square the diagram is drawn in, in pixels. */
	let boxWidth = 420;

	/** The filled horseshoe, which is painted rather than made into nodes. */
	let canvas = null;

	let hoverIndex = null;
	let showTrail = true;
	let showLocus = true;
	let zoomed = true;

	$: size = Math.max(MIN_SIZE, boxWidth || MIN_SIZE);
	$: plotWidth = Math.max(1, size - PADDING.left - PADDING.right);
	$: plotHeight = Math.max(1, size - PADDING.top - PADDING.bottom);
	$: span = Math.max(1, to - from);

	// Everything drawn is derived in statements that name the values they are
	// built from, because Svelte works out what a statement depends on from the
	// names it mentions rather than from what the functions it calls read. A
	// statement that only called a helper would run once, while the history was
	// still empty, and never again.
	$: points = pointsOf(readings, latest);
	$: domain = zoomed ? fittedDomain(points) : FULL_DOMAIN;

	$: locusPath = pathOf(LOCUS, domain, plotWidth, plotHeight);
	$: purplePath = segmentOf(LOCUS[LOCUS.length - 1], LOCUS[0], domain, plotWidth, plotHeight);
	$: planckianPath = pathOf(planckianCurve(), domain, plotWidth, plotHeight);
	$: trailPath = showTrail ? pathOf(points, domain, plotWidth, plotHeight, true) : '';

	$: xTicks = ticksOf(domain.x0, domain.x1);
	$: yTicks = ticksOf(domain.y0, domain.y1);

	$: placed = points.map((point, index) => ({
		...point,
		px: scaleX(point.x, domain, plotWidth),
		py: scaleY(point.y, domain, plotHeight),
		// The trail fades into the past so the direction of a change is
		// readable from a still picture: the bright end is now.
		opacity: points.length < 2 ? 1 : 0.18 + 0.82 * (index / (points.length - 1))
	}));

	$: shownIndex = hoverIndex !== null && hoverIndex < points.length ? hoverIndex : points.length - 1;
	$: shown = placed[shownIndex] ?? null;
	$: swatch = shown ? cssColour(shown.x, shown.y) : null;
	$: overloaded = points.some((point) => point.overload);

	// Painting is a side effect rather than a value, so it is written as a call
	// naming every input: the horseshoe is repainted when the size or the axes
	// change, and at no other time. It does not depend on the readings.
	$: paint(canvas, domain, plotWidth, plotHeight);

	/** The readings turned into plottable points, oldest first. */
	function pointsOf(history, newest) {
		const collected = [];

		for (const reading of history ?? []) {
			const point = pointOf(reading);
			if (point) {
				collected.push(point);
			}
		}

		const newestPoint = pointOf(newest);
		if (
			newestPoint &&
			(collected.length === 0 || newestPoint.t > collected[collected.length - 1].t)
		) {
			collected.push(newestPoint);
		}

		return collected;
	}

	/**
	 * One reading as a point, or `null` if it has no colour to draw.
	 *
	 * The device sends null coordinates for a reading taken in the dark, where
	 * the tristimulus values are all zero and the light has no colour because
	 * there is no light. Those readings still have an illuminance, which the
	 * illuminance chart draws; there is simply nothing to put on this diagram.
	 */
	function pointOf(reading) {
		if (!reading || typeof reading.t !== 'number') {
			return null;
		}

		const x = reading.cie_x;
		const y = reading.cie_y;
		if (typeof x !== 'number' || typeof y !== 'number') {
			return null;
		}
		if (!Number.isFinite(x) || !Number.isFinite(y) || y <= 0) {
			return null;
		}

		return {
			t: reading.t,
			x,
			y,
			lux: typeof reading.lux === 'number' && Number.isFinite(reading.lux) ? reading.lux : null,
			cct:
				typeof reading.cct_kelvin === 'number' && Number.isFinite(reading.cct_kelvin)
					? reading.cct_kelvin
					: null,
			// A flag averaged over a bucket of readings arrives as the fraction
			// of them that had it set, so anything above zero counts.
			overload: Number(reading.overload) > 0
		};
	}

	/**
	 * Axes just large enough to hold every reading, with room around them.
	 *
	 * Indoor light occupies a part of the diagram a few hundredths across, so
	 * on the full axes every reading of a day lands on the same few pixels. A
	 * minimum span keeps a single reading, or an unchanging one, from being
	 * blown up until the noise in the last digit fills the chart.
	 */
	function fittedDomain(collected) {
		if (collected.length === 0) {
			return FULL_DOMAIN;
		}

		let x0 = Infinity;
		let x1 = -Infinity;
		let y0 = Infinity;
		let y1 = -Infinity;

		for (const point of collected) {
			x0 = Math.min(x0, point.x);
			x1 = Math.max(x1, point.x);
			y0 = Math.min(y0, point.y);
			y1 = Math.max(y1, point.y);
		}

		// The diagram is drawn square, so both axes are given the same span:
		// a distance on the diagram means the same in either direction, which
		// is the whole point of plotting a colour space rather than two
		// unrelated numbers.
		const wanted = Math.max(0.04, (x1 - x0) * 1.35, (y1 - y0) * 1.35);
		const midX = (x0 + x1) / 2;
		const midY = (y0 + y1) / 2;

		return {
			x0: midX - wanted / 2,
			x1: midX + wanted / 2,
			y0: midY - wanted / 2,
			y1: midY + wanted / 2
		};
	}

	function scaleX(x, box, width) {
		return PADDING.left + ((x - box.x0) / (box.x1 - box.x0)) * width;
	}

	function scaleY(y, box, height) {
		return PADDING.top + (1 - (y - box.y0) / (box.y1 - box.y0)) * height;
	}

	/** An SVG path through a list of `{ x, y }` in chromaticity coordinates. */
	function pathOf(list, box, width, height, open = false) {
		if (!list || list.length < 2) {
			return '';
		}

		let path = '';
		for (const point of list) {
			const px = scaleX(point.x, box, width).toFixed(2);
			const py = scaleY(point.y, box, height).toFixed(2);
			path += path === '' ? `M${px} ${py}` : ` L${px} ${py}`;
		}

		return open ? path : `${path} Z`;
	}

	function segmentOf(a, b, box, width, height) {
		return `M${scaleX(a.x, box, width).toFixed(2)} ${scaleY(a.y, box, height).toFixed(2)} L${scaleX(
			b.x,
			box,
			width
		).toFixed(2)} ${scaleY(b.y, box, height).toFixed(2)}`;
	}

	/** The black-body locus, sampled closely enough to draw as a smooth curve. */
	function planckianCurve() {
		const curve = [];
		for (let kelvin = 1600; kelvin <= 20000; kelvin += kelvin < 5000 ? 100 : 500) {
			curve.push({ ...planckian(kelvin), kelvin });
		}
		return curve;
	}

	/**
	 * Where a black body at `kelvin` falls on the diagram.
	 *
	 * Kim's cubic fit to the Planckian locus, which is the same approximation
	 * used the other way round on the device to turn a measured colour into a
	 * colour temperature.
	 */
	function planckian(kelvin) {
		const t = Math.min(25000, Math.max(1667, kelvin));
		const inverse = 1000 / t;
		const x =
			t <= 4000
				? -0.2661239 * inverse ** 3 - 0.2343589 * inverse ** 2 + 0.8776956 * inverse + 0.17991
				: -3.0258469 * inverse ** 3 + 2.1070379 * inverse ** 2 + 0.2226347 * inverse + 0.24039;

		let y;
		if (t <= 2222) {
			y = -1.1063814 * x ** 3 - 1.3481102 * x ** 2 + 2.18555832 * x - 0.20219683;
		} else if (t <= 4000) {
			y = -0.9549476 * x ** 3 - 1.37418593 * x ** 2 + 2.09137015 * x - 0.16748867;
		} else {
			y = 3.081758 * x ** 3 - 5.8733867 * x ** 2 + 3.75112997 * x - 0.37001483;
		}

		return { x, y };
	}

	/**
	 * The approximate appearance of a chromaticity on this screen.
	 *
	 * The coordinates are turned back into a tristimulus value at a fixed
	 * brightness and then into sRGB, which is what the screen speaks. Most of
	 * the diagram is more saturated than a screen can show and comes out of
	 * that conversion with a negative component; lifting the whole triple until
	 * nothing is negative desaturates it towards white, which keeps the
	 * boundary between what the screen can and cannot show from appearing as a
	 * hard edge inside the horseshoe where no such edge exists.
	 */
	function srgb(x, y) {
		if (!(y > 0)) {
			return null;
		}

		// Back to a tristimulus value at a brightness of one, which is what makes
		// this the appearance of the point rather than of the light measured
		// there: the diagram has had brightness divided out of it.
		const bigX = x / y;
		const bigY = 1;
		const bigZ = (1 - x - y) / y;

		let r = 3.2406 * bigX - 1.5372 * bigY - 0.4986 * bigZ;
		let g = -0.9689 * bigX + 1.8758 * bigY + 0.0415 * bigZ;
		let b = 0.0557 * bigX - 0.204 * bigY + 1.057 * bigZ;

		const lift = Math.min(0, r, g, b);
		r -= lift;
		g -= lift;
		b -= lift;

		const peak = Math.max(r, g, b);
		if (!(peak > 0)) {
			return null;
		}

		return [encode(r / peak), encode(g / peak), encode(b / peak)];
	}

	/** The sRGB transfer function, which the screen undoes when it displays. */
	function encode(value) {
		const clamped = Math.min(1, Math.max(0, value));
		const encoded =
			clamped <= 0.0031308 ? 12.92 * clamped : 1.055 * clamped ** (1 / 2.4) - 0.055;
		return Math.round(encoded * 255);
	}

	function cssColour(x, y) {
		const rgb = srgb(x, y);
		return rgb === null ? null : `rgb(${rgb[0]}, ${rgb[1]}, ${rgb[2]})`;
	}

	/**
	 * Where the boundary of the diagram sits on one horizontal line, as the
	 * chromaticity of its left and right edges, or `null` above and below it.
	 *
	 * The region is convex, so a horizontal line either misses it or crosses it
	 * exactly twice and everything between those two crossings is inside. That
	 * is what makes the fill below a run per row rather than a test per pixel.
	 */
	function spanAt(y) {
		let lo = Infinity;
		let hi = -Infinity;

		for (let i = 0, j = LOCUS.length - 1; i < LOCUS.length; j = i++) {
			const a = LOCUS[i];
			const b = LOCUS[j];
			if (a.y > y !== b.y > y) {
				const x = a.x + ((b.x - a.x) * (y - a.y)) / (b.y - a.y);
				lo = Math.min(lo, x);
				hi = Math.max(hi, x);
			}
		}

		return hi > lo ? [lo, hi] : null;
	}

	/** Fill the visible part of the horseshoe, one row of pixels at a time. */
	function paint(node, box, width, height) {
		if (!node || width < 2 || height < 2) {
			return;
		}

		// Painted at the screen's own pixel density so the boundary is not
		// blurred on a high-density display, capped so a very dense one does
		// not quadruple the work for a difference nobody can see.
		const ratio = typeof devicePixelRatio === 'number' ? Math.min(devicePixelRatio, 2) : 1;
		const pixelWidth = Math.max(1, Math.round(width * ratio));
		const pixelHeight = Math.max(1, Math.round(height * ratio));

		node.width = pixelWidth;
		node.height = pixelHeight;
		node.style.width = `${width}px`;
		node.style.height = `${height}px`;

		const context = node.getContext('2d');
		if (!context) {
			return;
		}

		const image = context.createImageData(pixelWidth, pixelHeight);
		const data = image.data;
		const xSpan = box.x1 - box.x0;
		const ySpan = box.y1 - box.y0;

		for (let row = 0; row < pixelHeight; row += 1) {
			const y = box.y1 - ((row + 0.5) / pixelHeight) * ySpan;
			const inside = spanAt(y);
			if (inside === null) {
				continue;
			}

			const firstColumn = Math.max(
				0,
				Math.ceil(((inside[0] - box.x0) / xSpan) * pixelWidth - 0.5)
			);
			const lastColumn = Math.min(
				pixelWidth - 1,
				Math.floor(((inside[1] - box.x0) / xSpan) * pixelWidth - 0.5)
			);

			for (let column = firstColumn; column <= lastColumn; column += 1) {
				const x = box.x0 + ((column + 0.5) / pixelWidth) * xSpan;
				const rgb = srgb(x, y);
				if (rgb === null) {
					continue;
				}

				const offset = (row * pixelWidth + column) * 4;
				data[offset] = rgb[0];
				data[offset + 1] = rgb[1];
				data[offset + 2] = rgb[2];
				data[offset + 3] = 255;
			}
		}

		context.putImageData(image, 0, 0);
	}

	/** Round axis values covering `lo` to `hi`, about five of them. */
	function ticksOf(lo, hi) {
		const range = hi - lo;
		if (!(range > 0)) {
			return [];
		}

		const rough = range / 5;
		const magnitude = 10 ** Math.floor(Math.log10(rough));
		const step = [1, 2, 2.5, 5, 10].map((factor) => factor * magnitude).find((candidate) => candidate >= rough) ?? magnitude * 10;

		const ticks = [];
		for (let value = Math.ceil(lo / step) * step; value <= hi + step / 1000; value += step) {
			ticks.push(Number(value.toFixed(6)));
		}
		return ticks;
	}

	/** The reading nearest the pointer, if it is near enough to have been meant. */
	function onPointerMove(event) {
		const bounds = event.currentTarget.getBoundingClientRect();
		const px = event.clientX - bounds.left;
		const py = event.clientY - bounds.top;

		let nearest = null;
		let nearestDistance = Infinity;

		for (let index = 0; index < placed.length; index += 1) {
			const point = placed[index];
			const distance = (point.px - px) ** 2 + (point.py - py) ** 2;
			if (distance < nearestDistance) {
				nearestDistance = distance;
				nearest = index;
			}
		}

		hoverIndex = nearest !== null && nearestDistance <= 20 ** 2 ? nearest : null;
	}

	function formatNumber(value, decimals) {
		if (value === null || value === undefined || !Number.isFinite(value)) {
			return '—';
		}
		return value.toLocaleString(undefined, {
			minimumFractionDigits: decimals,
			maximumFractionDigits: decimals
		});
	}
</script>

<figure class="chart">
	<figcaption>
		<div class="heading">
			<h3>Colour of the light <span class="unit">(CIE 1931 x, y)</span></h3>

			<span class="meta">
				<span>{points.length} reading{points.length === 1 ? '' : 's'} drawn</span>
				{#if overloaded}
					<em class="warn">overloaded — some readings were cut off</em>
				{/if}
			</span>
		</div>

		<div class="controls">
			<label class="check" title="Rescale the axes to the readings instead of the whole diagram">
				<input type="checkbox" bind:checked={zoomed} />
				zoom to readings
			</label>
			<label class="check" title="Join the readings in the order they were taken">
				<input type="checkbox" bind:checked={showTrail} />
				trail
			</label>
			<label class="check" title="The curve anything glowing because it is hot sits on">
				<input type="checkbox" bind:checked={showLocus} />
				black-body locus
			</label>
		</div>
	</figcaption>

	<div class="body">
		<div class="diagram" bind:clientWidth={boxWidth}>
			<canvas
				bind:this={canvas}
				style="left: {PADDING.left}px; top: {PADDING.top}px"
				aria-hidden="true"
			></canvas>

			<svg
				width={size}
				height={size}
				viewBox="0 0 {size} {size}"
				role="img"
				aria-label="CIE 1931 chromaticity diagram of the measured light"
				on:pointermove={onPointerMove}
				on:pointerleave={() => (hoverIndex = null)}
			>
				<defs>
					<clipPath id="colour-plot">
						<rect x={PADDING.left} y={PADDING.top} width={plotWidth} height={plotHeight} />
					</clipPath>
				</defs>

				<rect
					class="frame"
					x={PADDING.left}
					y={PADDING.top}
					width={plotWidth}
					height={plotHeight}
				/>

				{#each xTicks as tick}
					<line
						class="grid"
						x1={scaleX(tick, domain, plotWidth)}
						x2={scaleX(tick, domain, plotWidth)}
						y1={PADDING.top}
						y2={PADDING.top + plotHeight}
					/>
					<text
						class="tick"
						x={scaleX(tick, domain, plotWidth)}
						y={PADDING.top + plotHeight + 14}
						text-anchor="middle">{tick}</text
					>
				{/each}

				{#each yTicks as tick}
					<line
						class="grid"
						x1={PADDING.left}
						x2={PADDING.left + plotWidth}
						y1={scaleY(tick, domain, plotHeight)}
						y2={scaleY(tick, domain, plotHeight)}
					/>
					<text
						class="tick"
						x={PADDING.left - 6}
						y={scaleY(tick, domain, plotHeight) + 3}
						text-anchor="end">{tick}</text
					>
				{/each}

				<text class="axis-label" x={PADDING.left + plotWidth / 2} y={size - 4} text-anchor="middle"
					>x</text
				>
				<text
					class="axis-label"
					x={12}
					y={PADDING.top + plotHeight / 2}
					text-anchor="middle"
					transform="rotate(-90 12 {PADDING.top + plotHeight / 2})">y</text
				>

				<g clip-path="url(#colour-plot)">
					<path class="boundary" d={locusPath} />
					<path class="boundary" d={purplePath} />

					{#if !zoomed}
						{#each LOCUS_NM as [nm, x, y]}
							{#if LABELLED_NM.includes(nm)}
								<circle
									class="locus-mark"
									cx={scaleX(x, domain, plotWidth)}
									cy={scaleY(y, domain, plotHeight)}
									r="1.6"
								/>
								<text
									class="locus-label"
									x={scaleX(x, domain, plotWidth) + (x < 0.3 ? -5 : 5)}
									y={scaleY(y, domain, plotHeight) + (y > 0.6 ? -4 : 4)}
									text-anchor={x < 0.3 ? 'end' : 'start'}>{nm}</text
								>
							{/if}
						{/each}
					{/if}

					{#if showLocus}
						<path class="planckian" d={planckianPath} />

						{#each ISOTHERMS as kelvin}
							{@const spot = planckian(kelvin)}
							<circle
								class="isotherm"
								cx={scaleX(spot.x, domain, plotWidth)}
								cy={scaleY(spot.y, domain, plotHeight)}
								r="2"
							/>
							<text
								class="isotherm-label"
								x={scaleX(spot.x, domain, plotWidth)}
								y={scaleY(spot.y, domain, plotHeight) - 6}
								text-anchor="middle">{kelvin >= 10000 ? `${kelvin / 1000}k` : kelvin}</text
							>
						{/each}

						{#each REFERENCES as reference}
							<g class="reference">
								<title>{reference.title}</title>
								<path
									d="M{scaleX(reference.x, domain, plotWidth) - 5} {scaleY(
										reference.y,
										domain,
										plotHeight
									)} h10 M{scaleX(reference.x, domain, plotWidth)} {scaleY(
										reference.y,
										domain,
										plotHeight
									) - 5} v10"
								/>
								<text
									x={scaleX(reference.x, domain, plotWidth) + 7}
									y={scaleY(reference.y, domain, plotHeight) + 11}>{reference.label}</text
								>
							</g>
						{/each}
					{/if}

					{#if showTrail && trailPath !== ''}
						<path class="trail" d={trailPath} />
					{/if}

					{#each placed as point, index}
						<circle
							class="reading"
							class:overload={point.overload}
							cx={point.px}
							cy={point.py}
							r={index === placed.length - 1 ? 4 : 2.4}
							opacity={point.opacity}
						/>
					{/each}

					{#if shown}
						<circle class="marker" cx={shown.px} cy={shown.py} r="7.5" />
					{/if}
				</g>
			</svg>
		</div>

		<div class="readout">
			{#if shown}
				<div class="swatch-row">
					<span
						class="swatch"
						style={swatch === null ? '' : `background: ${swatch}`}
						title="How this colour would look on this screen at full brightness"
					></span>
					<div>
						<p class="moment">
							{hoverIndex === null ? 'newest reading' : formatTime(shown.t, span)}
						</p>
						<p class="lux">
							{formatNumber(shown.lux, 2)} <span>lux</span>
						</p>
					</div>
				</div>

				<dl>
					<div>
						<dt>Colour temperature</dt>
						<dd>
							{#if shown.cct === null}
								<span class="absent">off the black-body curve</span>
							{:else}
								{formatNumber(shown.cct, 0)} <span>K</span>
							{/if}
						</dd>
					</div>
					<div>
						<dt>CIE x</dt>
						<dd>{formatNumber(shown.x, 4)}</dd>
					</div>
					<div>
						<dt>CIE y</dt>
						<dd>{formatNumber(shown.y, 4)}</dd>
					</div>
				</dl>

				{#if shown.overload}
					<p class="note warn">
						This reading overloaded the converter, so its colour is not a measurement.
					</p>
				{:else}
					<p class="note">
						Point at the diagram to read any reading in the range; the largest dot is the newest.
					</p>
				{/if}
			{:else}
				<p class="note">
					No colour to draw. The sensor reports coordinates only while there is light to measure;
					in the dark there is an illuminance but no colour, and the illuminance chart still shows
					it.
				</p>
			{/if}
		</div>
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

	.warn {
		color: #f97316;
		font-style: normal;
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

	/* The diagram and its readings side by side while there is room, and the
	   readings underneath when there is not. */
	.body {
		display: flex;
		flex-wrap: wrap;
		align-items: flex-start;
		gap: 0.75rem 1.25rem;
	}

	.diagram {
		position: relative;
		flex: 1 1 300px;
		max-width: 460px;
	}

	canvas {
		position: absolute;
		display: block;
	}

	svg {
		position: relative;
		display: block;
		overflow: visible;
	}

	.frame {
		fill: none;
		stroke: #1f2937;
	}

	.grid {
		stroke: #1e293b;
		stroke-width: 1;
	}

	.tick {
		fill: #64748b;
		font-size: 10px;
		font-variant-numeric: tabular-nums;
	}

	.axis-label {
		fill: #94a3b8;
		font-size: 11px;
	}

	/* The boundary is drawn dark rather than light: it separates the filled
	   horseshoe from the panel behind it, and a dark line reads against both. */
	.boundary {
		fill: none;
		stroke: #0b1120;
		stroke-width: 1.25;
		stroke-opacity: 0.85;
	}

	.locus-mark {
		fill: #0b1120;
		fill-opacity: 0.7;
	}

	.locus-label {
		fill: #0f172a;
		font-size: 9px;
		font-variant-numeric: tabular-nums;
	}

	.planckian {
		fill: none;
		stroke: #0b1120;
		stroke-width: 1.5;
		stroke-opacity: 0.75;
		stroke-dasharray: 4 3;
	}

	.isotherm {
		fill: #0b1120;
		fill-opacity: 0.8;
	}

	.isotherm-label {
		fill: #0b1120;
		font-size: 9px;
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}

	.reference path {
		fill: none;
		stroke: #0b1120;
		stroke-width: 1.25;
		stroke-opacity: 0.85;
	}

	.reference text {
		fill: #0b1120;
		font-size: 9px;
		font-weight: 600;
	}

	/* The readings are drawn in white with a dark edge, because they sit on a
	   background that is every colour at once and no single colour would stay
	   visible across it. */
	.trail {
		fill: none;
		stroke: #f8fafc;
		stroke-width: 1;
		stroke-opacity: 0.5;
		stroke-linejoin: round;
	}

	.reading {
		fill: #f8fafc;
		stroke: #0f172a;
		stroke-width: 1;
	}

	.reading.overload {
		fill: #f97316;
	}

	.marker {
		fill: none;
		stroke: #f8fafc;
		stroke-width: 1.5;
	}

	.readout {
		flex: 1 1 220px;
		min-width: 200px;
	}

	.swatch-row {
		display: flex;
		align-items: center;
		gap: 0.7rem;
	}

	.swatch {
		width: 54px;
		height: 54px;
		flex: none;
		border-radius: 8px;
		border: 1px solid #334155;
		background: #0f172a;
	}

	.moment {
		margin: 0;
		font-size: 0.75rem;
		color: #94a3b8;
		font-variant-numeric: tabular-nums;
	}

	.lux {
		margin: 0.1rem 0 0;
		font-size: 1.5rem;
		font-weight: 600;
		color: #e2e8f0;
		font-variant-numeric: tabular-nums;
	}

	.lux span {
		font-size: 0.85rem;
		font-weight: 400;
		color: #94a3b8;
	}

	dl {
		display: grid;
		gap: 0.25rem 0.75rem;
		margin: 0.75rem 0 0;
	}

	dl div {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 0.75rem;
		border-bottom: 1px solid #1f2937;
		padding-bottom: 0.2rem;
	}

	dt {
		font-size: 0.75rem;
		color: #94a3b8;
	}

	dd {
		margin: 0;
		font-size: 0.85rem;
		color: #e2e8f0;
		font-variant-numeric: tabular-nums;
	}

	dd span {
		font-size: 0.75rem;
		color: #94a3b8;
	}

	.absent {
		color: #64748b;
		font-size: 0.78rem;
	}

	.note {
		margin: 0.7rem 0 0;
		font-size: 0.72rem;
		line-height: 1.45;
		color: #64748b;
	}
</style>
