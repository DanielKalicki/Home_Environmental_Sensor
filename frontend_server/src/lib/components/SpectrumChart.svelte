<script>
	/**
	 * The AS7343's spectrum, over time and at one moment.
	 *
	 * The chart is two panels reading the same data. The upper one puts time
	 * along the bottom and wavelength up the side, and paints one cell per
	 * reading per channel: a horizontal band of colour is one channel over the
	 * whole range, so a light being switched on, a cloud passing or the sun
	 * moving is visible as a change in colour across the band. The lower panel
	 * is the spectrum at a single moment, one bar per channel; it follows the
	 * cursor over the upper panel, so scrubbing across the range plays back the
	 * shape of the spectrum as it changed, and it falls back to the newest
	 * reading when the cursor is elsewhere.
	 *
	 * What the numbers are: the raw counts the sensor's converter returned.
	 * They are not corrected for how differently the channels respond, so one
	 * channel standing above its neighbour does not by itself mean there is
	 * more light at that wavelength. What a channel does show reliably is how
	 * it itself changes, which is what the upper panel is for. Two settings
	 * qualify this. `Per channel` scales every band against its own range
	 * rather than against the largest count anywhere, which is what makes a
	 * weak channel's changes visible beside a strong one; the colours then mean
	 * something different in each band and cannot be compared across them.
	 * `Divide by gain` puts a range that crosses a change of gain back onto one
	 * scale, since doubling the gain doubles the counts without the light
	 * having changed. Both settings act on the upper panel only: the bars of the
	 * lower one are always drawn from zero on a plain scale, because a bar is
	 * read by its height and a height that is not measured from zero on a scale
	 * that is not even is not a reading of anything.
	 *
	 * The wavelength colours on the bars and the lines are the approximate
	 * appearance of that wavelength, which is a label. The colours inside the
	 * upper panel are a measurement, taken from the scale drawn beside it.
	 */
	import { SPECTRAL_WAVELENGTHS_NM } from '$lib/sensors.js';
	import { formatTime, timeAxisOf } from '$lib/timeAxis.js';

	/**
	 * The AS7343's readings over the drawn range, oldest first, as the history
	 * endpoint returns them. A long range arrives averaged into buckets, which
	 * is what keeps the number of cells painted below bounded.
	 */
	export let readings = [];

	/**
	 * The newest reading, which the status endpoint refreshes more often than
	 * the history is fetched. It is drawn as one more column when it is newer
	 * than everything in `readings`, so the right-hand edge is current.
	 */
	export let latest = null;

	/** The range the chart covers, in milliseconds since the Unix epoch. */
	export let from = 0;
	export let to = 1;

	const CHANNELS = SPECTRAL_WAVELENGTHS_NM.length;

	/**
	 * Drawn in real pixels, like the line charts: the width is measured from
	 * the element, so one user unit is one pixel and the labels are drawn at
	 * the size written here however many charts share the window.
	 */
	const HEIGHT = 300;
	const PADDING = { top: 30, right: 14, bottom: 30, left: 62 };
	const PLOT_HEIGHT = HEIGHT - PADDING.top - PADDING.bottom;

	const PROFILE_HEIGHT = 152;
	const PROFILE_PADDING = { top: 12, right: 14, bottom: 40, left: 62 };
	const PROFILE_PLOT_HEIGHT = PROFILE_HEIGHT - PROFILE_PADDING.top - PROFILE_PADDING.bottom;

	/**
	 * Longest a cell may be stretched to fill the time up to the next reading,
	 * as a multiple of the usual spacing between readings. A wider hole is a
	 * break in the record and is left as background rather than being coloured
	 * in with a reading that was never taken.
	 */
	const GAP_FACTOR = 2.5;

	/**
	 * The colour scale of the upper panel: matplotlib's viridis, sampled. It is
	 * read left to right as least to most light, and unlike a rainbow it rises
	 * evenly in lightness, so a step of colour is the same size of step in the
	 * reading wherever on the scale it falls.
	 */
	const VIRIDIS = [
		'#440154',
		'#472d7b',
		'#3b528b',
		'#2c728e',
		'#21918c',
		'#28ae80',
		'#5ec962',
		'#addc30',
		'#fde725'
	];

	/** The scale expanded once into 256 steps, rather than mixed per cell. */
	const RAMP = rampOf(VIRIDIS, 256);

	/** Measured width of the chart, in pixels; the fallback is for the server. */
	let width = 640;

	/** The upper panel's cells, which are painted rather than made into nodes. */
	let canvas = null;

	let hoverX = null;
	let hoverY = null;

	/** `heatmap` for the time-and-wavelength panel, `lines` for one line each. */
	let view = 'heatmap';
	/** `shared` scales every channel alike, `channel` each against its own. */
	let scaleMode = 'shared';
	/**
	 * Logarithmic to begin with. The channels differ by more than a hundredfold
	 * in how many counts the same light produces in them, so on a linear scale
	 * shared by all of them the weaker two thirds are painted the same dark
	 * colour throughout and their changes cannot be seen at all.
	 */
	let logScale = true;
	let perGain = false;
	/** A channel singled out by clicking its key, or `null` for all of them. */
	let focused = null;

	$: span = Math.max(1, to - from);
	$: plotWidth = Math.max(1, width - PADDING.left - PADDING.right);
	$: profileWidth = Math.max(1, width - PROFILE_PADDING.left - PROFILE_PADDING.right);
	$: rowHeight = PLOT_HEIGHT / CHANNELS;
	$: unit = perGain ? 'counts per unit of gain' : 'counts';

	// Everything drawn is derived in statements that name the values they are
	// built from, because Svelte works out what a statement depends on from the
	// names it mentions rather than from what the functions it calls read. A
	// statement that only called a helper would run once, while the history was
	// still empty, and never again.
	$: frames = framesOf(readings, latest, perGain);
	$: channelBounds = boundsOf(frames);
	$: overallBounds = overallOf(channelBounds);
	$: sharedRange = rangeOf(overallBounds, logScale, true);
	$: profileRange = roundedUp(rangeOf(overallBounds, false, true));
	$: ranges = channelBounds.map((bounds) =>
		scaleMode === 'channel' ? rangeOf(bounds, logScale, false) : sharedRange
	);

	$: columns = columnsOf(frames, from, span, plotWidth);
	$: timeAxis = frames.length > 0 ? timeAxisOf(from, to, PADDING.left, plotWidth) : [];
	$: linePaths =
		view === 'lines' ? linePathsOf(frames, ranges, logScale, from, span, plotWidth) : [];
	$: lineTicks = view === 'lines' ? valueTicks(sharedRange, logScale, scaleMode === 'channel') : [];
	$: profileTicks = valueTicks(profileRange, false, false);

	$: hoverTime =
		hoverX === null ? null : from + ((hoverX - PADDING.left) / plotWidth) * span;
	$: hoveredIndex = indexNear(frames, hoverTime, span);
	$: hoveredFrame = hoveredIndex === null ? null : frames[hoveredIndex];
	$: hoveredColumn = hoveredIndex === null ? null : (columns[hoveredIndex] ?? null);
	$: hoveredChannel = channelAt(hoverY, view);
	$: profileFrame = hoveredFrame ?? frames[frames.length - 1] ?? null;
	$: profileBars = barsOf(profileFrame, channelBounds, profileRange, profileWidth);
	$: saturationMarks = saturationOf(columns);

	// Painting is a side effect rather than a value, so it is written as a call
	// naming every input: the panel is repainted when the readings, the size or
	// any of the settings change, and at no other time.
	$: paint(canvas, columns, ranges, rowHeight, logScale, focused, width, view);

	/**
	 * The readings turned into what the panels draw: one frame per reading,
	 * holding the twelve channel values in the order of `SPECTRAL_WAVELENGTHS_NM`.
	 *
	 * A reading with no spectral fields at all is dropped rather than drawn as
	 * an empty column, and a single missing channel becomes a `null` that is
	 * left uncoloured.
	 */
	function framesOf(history, newest, dividedByGain) {
		const frames = [];

		for (const reading of history ?? []) {
			const frame = frameOf(reading, dividedByGain);
			if (frame) {
				frames.push(frame);
			}
		}

		const newestFrame = frameOf(newest, dividedByGain);
		if (newestFrame && (frames.length === 0 || newestFrame.t > frames[frames.length - 1].t)) {
			frames.push(newestFrame);
		}

		return frames;
	}

	function frameOf(reading, dividedByGain) {
		if (!reading || typeof reading.t !== 'number') {
			return null;
		}

		const gain = typeof reading.gain === 'number' && reading.gain > 0 ? reading.gain : null;
		const values = [];
		let measured = false;

		for (const nm of SPECTRAL_WAVELENGTHS_NM) {
			const counts = reading[`nm_${nm}`];
			if (typeof counts === 'number' && Number.isFinite(counts)) {
				values.push(dividedByGain && gain ? counts / gain : counts);
				measured = true;
			} else {
				values.push(null);
			}
		}

		if (!measured) {
			return null;
		}

		return {
			t: reading.t,
			gain,
			values,
			saturation: Math.max(flag(reading.analog_saturation), flag(reading.digital_saturation))
		};
	}

	/**
	 * A saturation flag as a number. The device sends it as a boolean; a range
	 * long enough to have been averaged into buckets carries the share of the
	 * bucket's readings that were flagged, which is already a number.
	 */
	function flag(value) {
		if (value === true) return 1;
		if (typeof value === 'number' && Number.isFinite(value)) return value;
		return 0;
	}

	/** The lowest and highest value each channel reached, and its smallest above zero. */
	function boundsOf(frames) {
		return SPECTRAL_WAVELENGTHS_NM.map((nm, index) => {
			let min = Infinity;
			let max = -Infinity;
			let minPositive = Infinity;

			for (const frame of frames) {
				const value = frame.values[index];
				if (value === null) {
					continue;
				}
				if (value < min) min = value;
				if (value > max) max = value;
				if (value > 0 && value < minPositive) minPositive = value;
			}

			if (!Number.isFinite(min)) {
				return null;
			}

			return { min, max, minPositive: Number.isFinite(minPositive) ? minPositive : max || 1 };
		});
	}

	/** The same, taken across every channel at once. */
	function overallOf(bounds) {
		let min = Infinity;
		let max = -Infinity;
		let minPositive = Infinity;

		for (const channel of bounds) {
			if (!channel) {
				continue;
			}
			min = Math.min(min, channel.min);
			max = Math.max(max, channel.max);
			minPositive = Math.min(minPositive, channel.minPositive);
		}

		if (!Number.isFinite(min)) {
			return null;
		}

		return { min, max, minPositive: Number.isFinite(minPositive) ? minPositive : max || 1 };
	}

	/**
	 * The span of values a scale covers.
	 *
	 * `fromZero` is for the scale shared by every channel, which starts at zero
	 * because a count of zero is a real reading and the height of a bar is only
	 * meaningful when measured from it. A per-channel scale starts at that
	 * channel's own lowest reading instead, which is what spreads a small
	 * change over the whole range of colour. A logarithmic scale cannot reach
	 * zero at all, so it starts at the smallest reading above it, and never
	 * spans more than the decades named here, which keeps one stray dark
	 * reading from flattening everything else.
	 */
	function rangeOf(bounds, log, fromZero) {
		if (!bounds) {
			return null;
		}

		if (log) {
			const hi = bounds.max > 0 ? bounds.max : 1;
			const decades = fromZero ? 1e4 : 1e3;
			const lo = Math.min(Math.max(bounds.minPositive, hi / decades), hi / 10);
			return { lo, hi };
		}

		const lo = fromZero ? Math.min(0, bounds.min) : bounds.min;
		const hi = bounds.max > lo ? bounds.max : lo + 1;
		return { lo, hi };
	}

	/** Where a value falls in a scale, from 0 at its bottom to 1 at its top. */
	function fractionOf(value, range, log) {
		if (!range || value === null) {
			return 0;
		}

		if (log) {
			const lo = Math.log(range.lo);
			const hi = Math.log(range.hi);
			if (!(hi > lo)) {
				return 0;
			}
			return clamp01((Math.log(Math.max(value, range.lo)) - lo) / (hi - lo));
		}

		if (!(range.hi > range.lo)) {
			return 0;
		}

		return clamp01((value - range.lo) / (range.hi - range.lo));
	}

	function clamp01(value) {
		return value < 0 ? 0 : value > 1 ? 1 : value;
	}

	/**
	 * Where each reading's column of cells starts and how wide it is.
	 *
	 * A column runs from the moment its reading was taken up to the next one,
	 * so consecutive readings tile the panel with no seam between them. A gap
	 * longer than the usual spacing is not filled: the column keeps its usual
	 * width and the background shows through the rest, which is how an
	 * interruption in the record stays visible as one.
	 *
	 * The edges are rounded to whole pixels. Two columns that met at a fraction
	 * of a pixel would each be painted half over the boundary and the pair would
	 * come out darker than either, which at a few pixels per reading shows up as
	 * a regular stripe across the whole panel that is not in the readings.
	 */
	function columnsOf(frames, rangeFrom, rangeSpan, availableWidth) {
		if (frames.length === 0) {
			return [];
		}

		const gaps = [];
		for (let i = 1; i < frames.length; i += 1) {
			gaps.push(frames[i].t - frames[i - 1].t);
		}
		gaps.sort((a, b) => a - b);
		const typical = gaps.length > 0 ? gaps[Math.floor(gaps.length / 2)] : rangeSpan / 60;
		const widest = Math.max(typical, 1) * GAP_FACTOR;

		return frames.map((frame, index) => {
			const next = frames[index + 1];
			const until = frame.t + (next ? Math.min(next.t - frame.t, widest) : Math.max(typical, 1));
			const x = Math.round(PADDING.left + ((frame.t - rangeFrom) / rangeSpan) * availableWidth);
			const end = Math.round(PADDING.left + ((until - rangeFrom) / rangeSpan) * availableWidth);

			return { frame, x, w: Math.max(end - x, 1) };
		});
	}

	/** The stretches of the range whose readings were cut off at the top. */
	function saturationOf(columns) {
		return columns
			.filter((column) => column.frame.saturation > 0)
			.map((column) => ({ x: column.x, w: column.w, opacity: 0.35 + 0.65 * column.frame.saturation }));
	}

	/** The upper panel, painted cell by cell. */
	function paint(target, columns, channelRanges, rowH, log, focus, boxWidth, mode) {
		if (!target || mode !== 'heatmap') {
			return;
		}

		const context = target.getContext('2d');
		if (!context) {
			return;
		}

		const ratio = typeof window === 'undefined' ? 1 : window.devicePixelRatio || 1;
		const pixelWidth = Math.max(1, Math.round(boxWidth * ratio));
		const pixelHeight = Math.round(HEIGHT * ratio);

		// Assigning either resets the canvas, so it is only done when the size
		// has actually changed rather than on every repaint.
		if (target.width !== pixelWidth || target.height !== pixelHeight) {
			target.width = pixelWidth;
			target.height = pixelHeight;
		}

		context.setTransform(ratio, 0, 0, ratio, 0, 0);
		context.clearRect(0, 0, boxWidth, HEIGHT);
		context.save();
		context.beginPath();
		context.rect(
			PADDING.left,
			PADDING.top,
			Math.max(1, boxWidth - PADDING.left - PADDING.right),
			PLOT_HEIGHT
		);
		context.clip();

		for (let index = 0; index < CHANNELS; index += 1) {
			const range = channelRanges[index];
			if (!range) {
				continue;
			}

			context.globalAlpha = focus === null || focus === index ? 1 : 0.2;
			const y = rowY(index, rowH);

			for (const column of columns) {
				const value = column.frame.values[index];
				if (value === null) {
					continue;
				}
				context.fillStyle = RAMP[Math.round(fractionOf(value, range, log) * (RAMP.length - 1))];
				context.fillRect(column.x, y, column.w, rowH);
			}
		}

		context.restore();
		context.globalAlpha = 1;
	}

	/** The top of a channel's band; the shortest wavelength sits at the bottom. */
	function rowY(index, rowH) {
		return PADDING.top + (CHANNELS - 1 - index) * rowH;
	}

	/** One line per channel, broken wherever the record is. */
	function linePathsOf(frames, channelRanges, log, rangeFrom, rangeSpan, availableWidth) {
		if (frames.length === 0) {
			return [];
		}

		const gaps = [];
		for (let i = 1; i < frames.length; i += 1) {
			gaps.push(frames[i].t - frames[i - 1].t);
		}
		gaps.sort((a, b) => a - b);
		const typical = gaps.length > 0 ? gaps[Math.floor(gaps.length / 2)] : 0;
		const maxGap = typical > 0 ? typical * (GAP_FACTOR + 1.5) : Infinity;

		return SPECTRAL_WAVELENGTHS_NM.map((nm, index) => {
			const range = channelRanges[index];
			let path = '';
			let penDown = false;

			for (let i = 0; i < frames.length; i += 1) {
				const value = frames[i].values[index];
				if (value === null || !range) {
					penDown = false;
					continue;
				}

				const broken = i > 0 && frames[i].t - frames[i - 1].t > maxGap;
				const x = (
					PADDING.left + ((frames[i].t - rangeFrom) / rangeSpan) * availableWidth
				).toFixed(2);
				const y = (
					PADDING.top + (1 - fractionOf(value, range, log)) * PLOT_HEIGHT
				).toFixed(2);

				path += !penDown || broken ? `M${x} ${y}` : `L${x} ${y}`;
				penDown = true;
			}

			return { nm, index, color: colorOf(nm), d: path };
		});
	}

	/**
	 * The labels up the side of the line panel.
	 *
	 * With every channel on its own scale there is no one value the side can be
	 * labelled with, so it is labelled as a share of each channel's own range
	 * instead, which is what the lines are then drawn against.
	 */
	function valueTicks(range, log, perChannel) {
		if (perChannel) {
			return [0, 0.25, 0.5, 0.75, 1].map((f) => ({ f, label: `${Math.round(f * 100)}%` }));
		}

		if (!range) {
			return [];
		}

		if (log) {
			const ticks = [];
			for (
				let exponent = Math.ceil(Math.log10(range.lo));
				exponent <= Math.floor(Math.log10(range.hi));
				exponent += 1
			) {
				const value = 10 ** exponent;
				ticks.push({ f: fractionOf(value, range, true), label: formatValue(value) });
			}
			return ticks;
		}

		const step = niceStep(range.hi - range.lo);
		const ticks = [];
		for (
			let value = Math.ceil(range.lo / step) * step;
			value <= range.hi + step / 1000;
			value += step
		) {
			ticks.push({ f: fractionOf(value, range, false), label: formatValue(value) });
		}
		return ticks;
	}

	/**
	 * A scale taken up to the next whole label, so the tallest bar has a little
	 * room above it and the topmost gridline is the edge of the panel rather
	 * than a line floating below it.
	 */
	function roundedUp(range) {
		if (!range) {
			return null;
		}

		const step = niceStep(range.hi - range.lo);
		return { lo: range.lo, hi: Math.ceil(range.hi / step + 0.0001) * step };
	}

	/** The largest of the amounts people count in that still gives five labels. */
	function niceStep(span) {
		if (!(span > 0) || !Number.isFinite(span)) {
			return 1;
		}

		const exponent = Math.floor(Math.log10(span)) - 1;

		for (let e = exponent; e <= exponent + 3; e += 1) {
			for (const factor of [1, 2, 2.5, 5]) {
				const step = factor * 10 ** e;
				if (span / step <= 5) {
					return step;
				}
			}
		}

		return span / 4;
	}

	/**
	 * The bars of the lower panel.
	 *
	 * They are always drawn against the plain scale shared by every channel,
	 * whatever the upper panel is set to: a spectrum whose bars each meant
	 * something different, or whose heights did not count from zero, would not
	 * be a spectrum. Behind each bar is a fainter one at the highest that
	 * channel reached anywhere in the range, so how much of its usual swing the
	 * moment on show is using can be seen at a glance.
	 */
	function barsOf(frame, bounds, range, availableWidth) {
		if (!frame || !range) {
			return [];
		}

		const slot = availableWidth / CHANNELS;
		const barWidth = Math.max(3, slot * 0.6);

		return SPECTRAL_WAVELENGTHS_NM.map((nm, index) => {
			const value = frame.values[index];
			const peak = bounds[index]?.max ?? null;

			return {
				nm,
				index,
				value,
				color: colorOf(nm),
				x: PROFILE_PADDING.left + slot * index + (slot - barWidth) / 2,
				width: barWidth,
				height: value === null ? 0 : fractionOf(value, range, false) * PROFILE_PLOT_HEIGHT,
				peakHeight: peak === null ? 0 : fractionOf(peak, range, false) * PROFILE_PLOT_HEIGHT,
				labelled: slot >= 34 || index % 2 === 0
			};
		});
	}

	/** The reading nearest the cursor, or `null` where there is none near it. */
	function indexNear(frames, t, rangeSpan) {
		if (t === null || frames.length === 0) {
			return null;
		}

		let best = null;
		let bestDistance = Infinity;

		for (let index = 0; index < frames.length; index += 1) {
			const distance = Math.abs(frames[index].t - t);
			if (distance < bestDistance) {
				bestDistance = distance;
				best = index;
			}
		}

		// Ignore a match further away than a fortieth of the visible range: the
		// cursor is then over a stretch that holds no readings at all.
		return bestDistance <= rangeSpan / 40 ? best : null;
	}

	/** The channel whose band the cursor is inside, in the upper panel. */
	function channelAt(y, mode) {
		if (y === null || mode !== 'heatmap') {
			return null;
		}

		const offset = y - PADDING.top;
		if (offset < 0 || offset > PLOT_HEIGHT) {
			return null;
		}

		const row = Math.min(CHANNELS - 1, Math.floor(offset / (PLOT_HEIGHT / CHANNELS)));
		return CHANNELS - 1 - row;
	}

	/** The colour scale expanded into `steps` fixed colours. */
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

			ramp.push(
				`rgb(${Math.round(from[0] + (to[0] - from[0]) * t)},` +
					`${Math.round(from[1] + (to[1] - from[1]) * t)},` +
					`${Math.round(from[2] + (to[2] - from[2]) * t)})`
			);
		}

		return ramp;
	}

	/**
	 * Roughly the colour a wavelength appears as, for labelling.
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

	/** As many decimals as the size of the number makes worth printing. */
	function formatValue(value) {
		if (value === null || value === undefined || !Number.isFinite(value)) {
			return '—';
		}

		if (value === 0) {
			return '0';
		}

		const magnitude = Math.abs(value);
		if (magnitude >= 100000) {
			return value.toExponential(2);
		}

		const decimals = magnitude >= 100 ? 0 : magnitude >= 10 ? 1 : magnitude >= 1 ? 2 : 3;
		return value.toLocaleString(undefined, {
			minimumFractionDigits: decimals,
			maximumFractionDigits: decimals
		});
	}

	function focus(index) {
		focused = focused === index ? null : index;
	}

	function onPointerMove(event) {
		const rect = event.currentTarget.getBoundingClientRect();
		hoverX = event.clientX - rect.left;
		hoverY = event.clientY - rect.top;
	}

	function onPointerLeave() {
		hoverX = null;
		hoverY = null;
	}
</script>

<figure class="chart">
	<figcaption>
		<div class="heading">
			<h3>Light spectrum over time <span class="unit">({unit})</span></h3>

			<span class="meta">
				{#if profileFrame}
					<span>gain ×{profileFrame.gain ?? '?'}</span>
					<span>{frames.length} reading{frames.length === 1 ? '' : 's'} drawn</span>
					{#if profileFrame.saturation > 0}
						<em class="saturated">saturated — counts are cut off</em>
					{/if}
				{/if}
			</span>
		</div>

		<div class="controls">
			<div class="segmented" role="group" aria-label="Panel">
				<button
					type="button"
					class:selected={view === 'heatmap'}
					title="Every channel as a band of colour, one column per reading"
					on:click={() => (view = 'heatmap')}
				>
					Heat map
				</button>
				<button
					type="button"
					class:selected={view === 'lines'}
					title="Every channel as a line against time"
					on:click={() => (view = 'lines')}
				>
					Lines
				</button>
			</div>

			<div class="segmented" role="group" aria-label="Scale">
				<button
					type="button"
					class:selected={scaleMode === 'shared'}
					title="One scale for every channel, so the channels can be compared"
					on:click={() => (scaleMode = 'shared')}
				>
					Shared scale
				</button>
				<button
					type="button"
					class:selected={scaleMode === 'channel'}
					title="Each channel against its own range, so a weak channel's changes show"
					on:click={() => (scaleMode = 'channel')}
				>
					Per channel
				</button>
			</div>

			<label class="check" title="Spread the small readings out over more of the scale">
				<input type="checkbox" bind:checked={logScale} />
				logarithmic
			</label>

			<label
				class="check"
				title="Divide the counts by the gain they were taken at, so a change of gain is not read as a change of light"
			>
				<input type="checkbox" bind:checked={perGain} />
				divide by gain
			</label>

			{#if view === 'heatmap'}
				<div class="scale-key" title="What the colours in the panel mean">
					<span>{scaleMode === 'channel' ? 'own low' : formatValue(sharedRange?.lo ?? 0)}</span>
					<svg class="ramp" width="110" height="10" aria-hidden="true">
						<defs>
							<linearGradient id="spectrum-viridis" x1="0" y1="0" x2="1" y2="0">
								{#each VIRIDIS as stop, index}
									<stop offset={index / (VIRIDIS.length - 1)} stop-color={stop} />
								{/each}
							</linearGradient>
						</defs>
						<rect width="110" height="10" rx="2" fill="url(#spectrum-viridis)" />
					</svg>
					<span>{scaleMode === 'channel' ? 'own high' : formatValue(sharedRange?.hi ?? 0)}</span>
				</div>
			{/if}
		</div>

		<ul class="channels" aria-label="Channels">
			{#each SPECTRAL_WAVELENGTHS_NM as nm, index}
				<li>
					<button
						type="button"
						class:selected={focused === index}
						class:dimmed={focused !== null && focused !== index}
						aria-pressed={focused === index}
						title="Show only this channel"
						on:click={() => focus(index)}
					>
						<span class="swatch" style:background={colorOf(nm)}></span>
						{nm}
					</button>
				</li>
			{/each}
		</ul>
	</figcaption>

	<div class="plot" bind:clientWidth={width}>
		{#if view === 'heatmap'}
			<canvas bind:this={canvas} style="height: {HEIGHT}px"></canvas>
		{/if}

		<svg
			viewBox="0 0 {width} {HEIGHT}"
			{width}
			height={HEIGHT}
			role="img"
			aria-label="Spectral channels over time"
			on:pointermove={onPointerMove}
			on:pointerleave={onPointerLeave}
		>
			{#if frames.length > 0}
				{#if view === 'lines'}
					{#each lineTicks as tick}
						<line
							class="grid"
							x1={PADDING.left}
							x2={width - PADDING.right}
							y1={PADDING.top + (1 - tick.f) * PLOT_HEIGHT}
							y2={PADDING.top + (1 - tick.f) * PLOT_HEIGHT}
						/>
						<text
							class="axis"
							x={PADDING.left - 8}
							y={PADDING.top + (1 - tick.f) * PLOT_HEIGHT + 4}
							text-anchor="end"
						>
							{tick.label}
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

					{#each linePaths as line}
						<path
							d={line.d}
							fill="none"
							stroke={line.color}
							stroke-width={focused === line.index ? 2.4 : 1.5}
							opacity={focused === null || focused === line.index ? 0.95 : 0.18}
						/>
					{/each}
				{:else}
					<!-- The cells themselves are painted onto the canvas below; only
					     the labels around them are drawn as shapes. -->
					<text class="axis corner" x={PADDING.left - 8} y={PADDING.top - 8} text-anchor="end">
						nm
					</text>

					{#each SPECTRAL_WAVELENGTHS_NM as nm, index}
						<text
							class="axis"
							class:dimmed={focused !== null && focused !== index}
							x={PADDING.left - 8}
							y={rowY(index, rowHeight) + rowHeight / 2 + 4}
							text-anchor="end"
						>
							{nm}
						</text>
					{/each}
				{/if}

				{#if saturationMarks.length > 0}
					<text class="axis small" x={PADDING.left - 8} y="12" text-anchor="end">clipped</text>
					{#each saturationMarks as mark}
						<rect
							class="saturation"
							x={mark.x}
							y="4"
							width={mark.w}
							height="6"
							opacity={mark.opacity}
						>
							<title>readings here were cut off at the top of the sensor's range</title>
						</rect>
					{/each}
				{/if}

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

				{#if hoveredFrame}
					{#if view === 'heatmap' && hoveredColumn && hoveredChannel !== null}
						<rect
							class="cell-cursor"
							x={hoveredColumn.x - 0.5}
							y={rowY(hoveredChannel, rowHeight)}
							width={hoveredColumn.w + 1}
							height={rowHeight}
						/>
					{/if}

					<line
						class="cursor"
						x1={hoveredColumn ? hoveredColumn.x + hoveredColumn.w / 2 : hoverX}
						x2={hoveredColumn ? hoveredColumn.x + hoveredColumn.w / 2 : hoverX}
						y1={PADDING.top}
						y2={PADDING.top + PLOT_HEIGHT}
					/>

					{#if view === 'lines'}
						{#each SPECTRAL_WAVELENGTHS_NM as nm, index}
							{#if hoveredFrame.values[index] !== null && ranges[index] && (focused === null || focused === index)}
								<circle
									cx={PADDING.left + ((hoveredFrame.t - from) / span) * plotWidth}
									cy={PADDING.top +
										(1 - fractionOf(hoveredFrame.values[index], ranges[index], logScale)) *
											PLOT_HEIGHT}
									r="3"
									fill="#0f172a"
									stroke={colorOf(nm)}
									stroke-width="1.5"
								/>
							{/if}
						{/each}
					{/if}
				{/if}
			{:else}
				<text class="empty" x={width / 2} y={HEIGHT / 2} text-anchor="middle">
					no spectral readings in this range
				</text>
			{/if}
		</svg>

		{#if hoveredFrame && hoverX !== null}
			<div
				class="tooltip"
				class:flipped={hoverX > width - 200}
				style="left: {hoverX}px; top: {Math.min(
					Math.max(hoverY ?? PADDING.top, PADDING.top),
					HEIGHT - 76
				)}px"
			>
				<strong>{formatTime(hoveredFrame.t, span)}</strong>
				{#if view === 'heatmap' && hoveredChannel !== null}
					<span class="tip-row">
						<span class="swatch" style:background={colorOf(SPECTRAL_WAVELENGTHS_NM[hoveredChannel])}
						></span>
						{SPECTRAL_WAVELENGTHS_NM[hoveredChannel]} nm
						<b>{formatValue(hoveredFrame.values[hoveredChannel])}</b>
					</span>
				{:else if focused !== null}
					<span class="tip-row">
						<span class="swatch" style:background={colorOf(SPECTRAL_WAVELENGTHS_NM[focused])}></span>
						{SPECTRAL_WAVELENGTHS_NM[focused]} nm
						<b>{formatValue(hoveredFrame.values[focused])}</b>
					</span>
				{/if}
				<span class="tip-note">
					gain ×{hoveredFrame.gain ?? '?'}{hoveredFrame.saturation > 0 ? ' · saturated' : ''}
				</span>
			</div>
		{/if}
	</div>

	{#if profileFrame}
		<div class="profile">
			<h4>
				Spectrum at one moment
				<span class="moment">
					{hoveredFrame ? formatTime(profileFrame.t, span) : `newest · ${formatTime(profileFrame.t, span)}`}
				</span>
			</h4>

			<svg
				viewBox="0 0 {width} {PROFILE_HEIGHT}"
				{width}
				height={PROFILE_HEIGHT}
				role="img"
				aria-label="Spectrum at the selected moment"
			>
				{#each profileTicks as tick}
					<line
						class="grid"
						x1={PROFILE_PADDING.left}
						x2={width - PROFILE_PADDING.right}
						y1={PROFILE_PADDING.top + (1 - tick.f) * PROFILE_PLOT_HEIGHT}
						y2={PROFILE_PADDING.top + (1 - tick.f) * PROFILE_PLOT_HEIGHT}
					/>
					<text
						class="axis"
						x={PROFILE_PADDING.left - 8}
						y={PROFILE_PADDING.top + (1 - tick.f) * PROFILE_PLOT_HEIGHT + 4}
						text-anchor="end"
					>
						{tick.label}
					</text>
				{/each}

				{#each profileBars as bar}
					<rect
						class="peak"
						x={bar.x}
						y={PROFILE_PADDING.top + PROFILE_PLOT_HEIGHT - bar.peakHeight}
						width={bar.width}
						height={bar.peakHeight}
						rx="2"
						opacity={focused === null || focused === bar.index ? 1 : 0.3}
					/>
					<rect
						x={bar.x}
						y={PROFILE_PADDING.top + PROFILE_PLOT_HEIGHT - bar.height}
						width={bar.width}
						height={bar.height}
						fill={bar.color}
						rx="2"
						opacity={focused === null || focused === bar.index ? 1 : 0.25}
					>
						<title>
							{bar.nm} nm: {formatValue(bar.value)}
							{unit} · highest in this range {formatValue(channelBounds[bar.index]?.max ?? null)}
						</title>
					</rect>
					{#if bar.labelled}
						<text
							class="axis small"
							class:dimmed={focused !== null && focused !== bar.index}
							x={bar.x + bar.width / 2}
							y={PROFILE_HEIGHT - 24}
							text-anchor="middle"
						>
							{bar.nm}
						</text>
					{/if}
				{/each}

				<line
					class="axis-line"
					x1={PROFILE_PADDING.left}
					x2={width - PROFILE_PADDING.right}
					y1={PROFILE_PADDING.top + PROFILE_PLOT_HEIGHT}
					y2={PROFILE_PADDING.top + PROFILE_PLOT_HEIGHT}
				/>

				<text class="axis" x={width / 2} y={PROFILE_HEIGHT - 6} text-anchor="middle">
					wavelength (nm) — bars count from zero; the faint bar behind each is the highest that
					channel reached in this range
				</text>
			</svg>
		</div>
	{/if}
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

	h4 {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 0.5rem;
		margin: 0.35rem 0 0;
		font-size: 0.82rem;
		font-weight: 600;
		color: #cbd5f5;
	}

	.moment {
		font-weight: 400;
		font-variant-numeric: tabular-nums;
		color: #94a3b8;
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

	.saturated {
		color: #f97316;
		font-style: normal;
	}

	.controls {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.4rem 0.9rem;
	}

	/* The two settings that pick one of a fixed pair are drawn as joined
	   buttons rather than as a dropdown, so which one is in force is readable
	   without opening anything. */
	.segmented {
		display: inline-flex;
		border: 1px solid #1f2937;
		border-radius: 999px;
		overflow: hidden;
	}

	.segmented button {
		padding: 0.25rem 0.7rem;
		border: 0;
		background: #0f172a;
		color: #94a3b8;
		font: inherit;
		font-size: 0.75rem;
		cursor: pointer;
	}

	.segmented button + button {
		border-left: 1px solid #1f2937;
	}

	.segmented button:hover {
		color: #e2e8f0;
	}

	.segmented button.selected {
		background: #1e293b;
		color: #e2e8f0;
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

	.channels {
		display: flex;
		flex-wrap: wrap;
		gap: 0.25rem;
		margin: 0;
		padding: 0;
		list-style: none;
	}

	.channels button {
		display: inline-flex;
		align-items: center;
		gap: 0.3rem;
		padding: 0.15rem 0.45rem;
		border: 1px solid transparent;
		border-radius: 999px;
		background: #0f172a;
		color: #cbd5f5;
		font: inherit;
		font-size: 0.72rem;
		font-variant-numeric: tabular-nums;
		cursor: pointer;
	}

	.channels button:hover {
		border-color: #334155;
	}

	.channels button.selected {
		border-color: #4f9cf9;
		color: #e2e8f0;
	}

	.channels button.dimmed {
		opacity: 0.45;
	}

	.swatch {
		width: 0.55rem;
		height: 0.55rem;
		border-radius: 2px;
	}

	/* The cells are painted onto a canvas and the labels drawn as an SVG on top
	   of it: a long range holds thousands of cells, which are far cheaper to
	   paint than to keep as that many elements in the page. */
	.plot {
		position: relative;
		width: 100%;
	}

	canvas {
		position: absolute;
		inset: 0;
		width: 100%;
	}

	svg {
		display: block;
		position: relative;
		width: 100%;
	}

	.grid {
		stroke: #1f2937;
		stroke-width: 1;
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

	.axis.small {
		font-size: 10.5px;
	}

	.axis.corner {
		fill: #64748b;
		font-size: 10.5px;
	}

	.axis.dimmed {
		opacity: 0.4;
	}

	.saturation {
		fill: #f97316;
	}

	.cursor {
		stroke: #e2e8f0;
		stroke-width: 1;
		stroke-dasharray: 3 3;
		opacity: 0.7;
	}

	.cell-cursor {
		fill: none;
		stroke: #e2e8f0;
		stroke-width: 1.5;
	}

	.peak {
		fill: #1e293b;
	}

	.empty {
		fill: #475569;
		font-size: 14px;
		font-family: inherit;
	}

	.tooltip {
		position: absolute;
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		padding: 0.35rem 0.55rem;
		transform: translate(0.75rem, 0);
		border: 1px solid #334155;
		border-radius: 6px;
		background: rgba(15, 23, 42, 0.94);
		font-size: 0.75rem;
		color: #e2e8f0;
		font-variant-numeric: tabular-nums;
		pointer-events: none;
		white-space: nowrap;
	}

	.tooltip.flipped {
		transform: translate(calc(-100% - 0.75rem), 0);
	}

	.tip-row {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		color: #cbd5f5;
	}

	.tip-note {
		color: #64748b;
	}
</style>
