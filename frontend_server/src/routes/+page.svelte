<script>
	/**
	 * The dashboard.
	 *
	 * It reads this server's own endpoints and never contacts the device:
	 * `/api/status` for the newest values and the state of the collection,
	 * `/api/history` for the range being drawn, and `/api/thermal` for the last
	 * picture the thermal camera took. The history a chart shows can therefore
	 * reach further back than the day the device itself retains.
	 */
	import { onMount } from 'svelte';

	import LineChart from '$lib/components/LineChart.svelte';
	import ColorChart from '$lib/components/ColorChart.svelte';
	import SpectrumChart from '$lib/components/SpectrumChart.svelte';
	import ThermalImage from '$lib/components/ThermalImage.svelte';
	import { CHARTS, SENSOR_LABELS, SENSORS, seriesValue } from '$lib/sensors.js';

	/** Selectable spans of history, as milliseconds back from now. */
	const RANGES = [
		{ label: '15 min', ms: 15 * 60 * 1000 },
		{ label: '1 hour', ms: 60 * 60 * 1000 },
		{ label: '6 hours', ms: 6 * 60 * 60 * 1000 },
		{ label: '24 hours', ms: 24 * 60 * 60 * 1000 },
		{ label: '7 days', ms: 7 * 24 * 60 * 60 * 1000 },
		{ label: '30 days', ms: 30 * 24 * 60 * 60 * 1000 }
	];

	/** How often the current-value cards are refreshed. */
	const STATUS_INTERVAL_MS = 5000;
	/** How often the charts are redrawn from freshly fetched history. */
	const HISTORY_INTERVAL_MS = 20000;
	/**
	 * How often the thermal image is refreshed. It is fetched separately from
	 * the status because it is 768 temperatures, and there is no point asking
	 * for it faster than the device takes one.
	 */
	const THERMAL_INTERVAL_MS = 10000;
	/**
	 * How often the displayed age of the thermal image is recomputed.
	 *
	 * The age has to be driven by a clock of its own rather than by the fetch
	 * that brings the image in. Measured from the fetch, it is a single reading
	 * taken the instant the image arrived and then held until the next one
	 * arrives, so it never counts up; and because the browser asks every 10 s
	 * while the camera takes one every 10.15 s, the image waiting for it is
	 * always about equally old, so that held reading is the same number every
	 * time. The result is an age that sits at a few seconds indefinitely while
	 * the image behind it is in fact ageing normally.
	 */
	const THERMAL_AGE_TICK_MS = 1000;
	/** Points requested per sensor; the server averages the rest into these. */
	const CHART_POINTS = 700;

	/** The status and one hour of history the server rendered the page with. */
	export let data;

	let selectedRange = RANGES[1];
	let status = data.status;
	let history = data.history;
	let historyError = null;
	let loadingHistory = false;

	/** The last thermal image fetched, and the moment its age is measured against. */
	let thermal = null;
	let thermalNow = Date.now();

	/**
	 * Sensors the reader has clicked off in a chart's legend. The flag is per
	 * sensor rather than per series, so turning off the SCD41 hides it from
	 * every chart it appears on (temperature, humidity, CO₂) with one click,
	 * instead of leaving it live on the charts that were not clicked. It is
	 * kept in `localStorage` so a reader who hides the noisy sensors does not
	 * have to redo it on every visit.
	 */
	const DISABLED_SENSORS_KEY = 'disabledSensors';
	let disabledSensors = new Set();

	$: latest = status?.sensors ?? {};

	// `history` and `disabledSensors` are named here rather than only inside
	// `chartsFrom`, because Svelte works out what a reactive statement depends
	// on from the names it mentions, not from what the functions it calls
	// happen to read. A statement that only called a helper would be computed
	// once, while the history was still empty, and never again.
	$: charts = chartsFrom(history, disabledSensors);

	/** Every chart, with its series turned into points ready to draw. */
	function chartsFrom(fetched, disabled) {
		return CHARTS.map((chart) => ({
			...chart,
			drawn: chart.series.map((series) => ({
				sensor: series.sensor,
				label: series.label,
				color: series.color,
				points: pointsOf(fetched, series),
				hidden: disabled.has(series.sensor)
			}))
		}));
	}

	/** Flips whether `sensor` is hidden from the charts, and remembers it. */
	function toggleSensor(sensor) {
		const next = new Set(disabledSensors);
		if (next.has(sensor)) {
			next.delete(sensor);
		} else {
			next.add(sensor);
		}
		disabledSensors = next;

		if (typeof localStorage !== 'undefined') {
			localStorage.setItem(DISABLED_SENSORS_KEY, JSON.stringify([...next]));
		}
	}

	/** The `{ t, v }` points of one chart series, from the fetched history. */
	function pointsOf(fetched, series) {
		const readings = fetched?.sensors?.[series.sensor] ?? [];
		const points = [];

		for (const reading of readings) {
			const value = seriesValue(reading, series);
			if (value !== null) {
				points.push({ t: reading.t, v: value });
			}
		}

		return points;
	}

	async function loadStatus() {
		try {
			const response = await fetch('/api/status');
			if (!response.ok) {
				throw new Error(`status request failed: ${response.status}`);
			}
			status = await response.json();
		} catch (error) {
			// The collection keeps running in the server; only this page's view
			// of it is stale, and the next tick replaces it.
			console.warn(error);
		}
	}

	async function loadHistory() {
		loadingHistory = true;
		const to = Date.now();
		const from = to - selectedRange.ms;

		try {
			const query = new URLSearchParams({
				from: String(from),
				to: String(to),
				points: String(CHART_POINTS)
			});
			const response = await fetch(`/api/history?${query}`);
			if (!response.ok) {
				throw new Error(`history request failed: ${response.status}`);
			}
			history = await response.json();
			historyError = null;
		} catch (error) {
			historyError = error instanceof Error ? error.message : String(error);
		} finally {
			loadingHistory = false;
		}
	}

	function selectRange(range) {
		selectedRange = range;
		void loadHistory();
	}
	async function loadThermal() {
		try {
			const response = await fetch('/api/thermal');
			if (!response.ok) {
				throw new Error(`thermal request failed: ${response.status}`);
			}
			thermal = await response.json();
		} catch (error) {
			// Same as the status: the collection is unaffected, only this view of
			// it is stale until the next tick.
			console.warn(error);
		}
	}

	function formatNumber(value, decimals = 1) {
		if (value === null || value === undefined || !Number.isFinite(value)) {
			return '—';
		}
		return value.toLocaleString(undefined, {
			minimumFractionDigits: decimals,
			maximumFractionDigits: decimals
		});
	}

	function formatAge(timestamp) {
		if (!timestamp) {
			return 'never';
		}

		const seconds = Math.max(0, Math.round((Date.now() - timestamp) / 1000));
		if (seconds < 60) return `${seconds} s ago`;
		if (seconds < 3600) return `${Math.round(seconds / 60)} min ago`;
		if (seconds < 86400) return `${Math.round(seconds / 3600)} h ago`;
		return `${Math.round(seconds / 86400)} d ago`;
	}

	function formatDuration(ms) {
		if (!ms && ms !== 0) {
			return '—';
		}

		const totalMinutes = Math.floor(ms / 60000);
		const days = Math.floor(totalMinutes / 1440);
		const hours = Math.floor((totalMinutes % 1440) / 60);
		const minutes = totalMinutes % 60;

		return days > 0 ? `${days} d ${hours} h` : `${hours} h ${minutes} min`;
	}

	/** The BME690's calibration stage, which qualifies everything gas-derived. */
	function accuracyLabel(accuracy) {
		return ['unreliable', 'calibrating', 'calibrated', 'high accuracy'][accuracy] ?? 'unknown';
	}

	onMount(() => {
		try {
			const saved = JSON.parse(localStorage.getItem(DISABLED_SENSORS_KEY) ?? '[]');
			if (Array.isArray(saved) && saved.length > 0) {
				disabledSensors = new Set(saved);
			}
		} catch (error) {
			// A corrupt or missing value just leaves every sensor shown.
			console.warn(error);
		}

		void loadStatus();
		void loadHistory();
		void loadThermal();

		const statusTimer = setInterval(loadStatus, STATUS_INTERVAL_MS);
		const historyTimer = setInterval(loadHistory, HISTORY_INTERVAL_MS);
		const thermalTimer = setInterval(loadThermal, THERMAL_INTERVAL_MS);
		const thermalAgeTimer = setInterval(() => {
			thermalNow = Date.now();
		}, THERMAL_AGE_TICK_MS);

		return () => {
			clearInterval(statusTimer);
			clearInterval(historyTimer);
			clearInterval(thermalTimer);
			clearInterval(thermalAgeTimer);
		};
	});
</script>

<svelte:head>
	<title>Environmental Sensor</title>
</svelte:head>

<main>
	<header>
		<div>
			<h1>Home Environmental Sensor</h1>
			<p class="subtitle">
				Collected from <code>{status?.deviceUrl ?? 'the device'}</code> by this server, every
				{Math.round((status?.pollIntervalMs ?? 5000) / 1000)} s.
			</p>
		</div>

		<div class="connection" class:offline={status && !status.deviceOnline}>
			<span class="dot"></span>
			{#if !status}
				connecting
			{:else if status.deviceOnline}
				device reachable · up {formatDuration(status.deviceUptimeMs)}
			{:else}
				device unreachable · {status.lastError ?? 'no answer'}
			{/if}
		</div>
	</header>

	{#if status && !status.deviceOnline}
		<p class="notice">
			The device is not answering. The readings below are the newest that were collected before it
			stopped; the server keeps retrying and will fill in what it missed once the device is back,
			as far as the device still retains it.
		</p>
	{/if}

	<section class="cards">
		<article>
			<h2>CO₂</h2>
			<p class="value">{formatNumber(latest.scd41?.latest?.co2_ppm, 0)} <span>ppm</span></p>
			<p class="age">SCD41 · {formatAge(latest.scd41?.latest?.t)}</p>
		</article>
		<article>
			<h2>Temperature</h2>
			<p class="value">
				{formatNumber(latest.scd41?.latest?.temperature_celsius, 1)} <span>°C</span>
			</p>
			<p class="age">SCD41 · {formatAge(latest.scd41?.latest?.t)}</p>
		</article>
		<article>
			<h2>Humidity</h2>
			<p class="value">
				{formatNumber(latest.scd41?.latest?.humidity_percent, 1)} <span>%</span>
			</p>
			<p class="age">SCD41 · {formatAge(latest.scd41?.latest?.t)}</p>
		</article>
		<article>
			<h2>Pressure</h2>
			<p class="value">
				{formatNumber((latest.bme690?.latest?.pressure_pascals ?? 0) / 100 || null, 1)}
				<span>hPa</span>
			</p>
			<p class="age">BME690 · {formatAge(latest.bme690?.latest?.t)}</p>
		</article>
		<article>
			<h2>PM2.5</h2>
			<p class="value">{formatNumber(latest.sps30?.latest?.pm2_5, 1)} <span>µg/m³</span></p>
			<p class="age">SPS30 · {formatAge(latest.sps30?.latest?.t)}</p>
		</article>
		<article>
			<h2>PM10</h2>
			<p class="value">{formatNumber(latest.sps30?.latest?.pm10, 1)} <span>µg/m³</span></p>
			<p class="age">SPS30 · {formatAge(latest.sps30?.latest?.t)}</p>
		</article>
		<article>
			<h2>Illuminance</h2>
			<p class="value">{formatNumber(latest.opt4048?.latest?.lux, 1)} <span>lux</span></p>
			<p class="age">OPT4048 · {formatAge(latest.opt4048?.latest?.t)}</p>
		</article>
		<article>
			<h2>Colour temperature</h2>
			<p class="value">
				{formatNumber(latest.opt4048?.latest?.cct_kelvin, 0)} <span>K</span>
			</p>
			<!-- The device sends no colour temperature for light too far off the
			     black-body curve for one to mean anything, and none at all in the
			     dark; the dash the card then shows is the honest answer. -->
			<p class="age">OPT4048 · {formatAge(latest.opt4048?.latest?.t)}</p>
		</article>
		<article class:unqualified={!latest.bme690?.latest?.iaq_accuracy}>
			<h2>Air quality index</h2>
			<p class="value">{formatNumber(latest.bme690?.latest?.iaq, 0)}</p>
			<p class="age">
				BME690 · {accuracyLabel(latest.bme690?.latest?.iaq_accuracy)}
			</p>
		</article>
		<article class:unqualified={!latest.bme690?.latest?.iaq_accuracy}>
			<h2>TVOC</h2>
			<p class="value">
				{formatNumber(latest.bme690?.latest?.tvoc_equivalent_ppb, 2)} <span>ppb</span>
			</p>
			<p class="age">BME690 · estimated from the gas signal</p>
		</article>
	</section>

	{#if latest.bme690?.latest && latest.bme690.latest.iaq_accuracy === 0}
		<p class="notice subtle">
			The BME690's gas readings are placeholders until it has learned a baseline, so the air quality
			index and TVOC above mean nothing yet.
		</p>
	{/if}

	<nav class="ranges" aria-label="History range">
		{#each RANGES as range}
			<button
				type="button"
				class:selected={range.ms === selectedRange.ms}
				on:click={() => selectRange(range)}
			>
				{range.label}
			</button>
		{/each}
		<span class="range-note">
			{#if loadingHistory}
				loading…
			{:else if historyError}
				{historyError}
			{:else if status?.oldestReadingAt}
				stored since {new Date(status.oldestReadingAt).toLocaleString()}
			{/if}
		</span>
	</nav>

	<section class="charts">
		{#each charts as chart (chart.id)}
			<LineChart
				title={chart.title}
				unit={chart.unit}
				decimals={chart.decimals ?? 0}
				series={chart.drawn}
				from={history?.from ?? Date.now() - selectedRange.ms}
				to={history?.to ?? Date.now()}
				on:toggle={(event) => toggleSensor(event.detail)}
			/>
		{/each}

		<!-- The spectrum is twelve channels against time rather than one line, so
		     it is given the whole width of the grid instead of one of its cells. -->
		<div class="wide">
			<SpectrumChart
				readings={history?.sensors?.as7343 ?? []}
				latest={latest.as7343?.latest ?? null}
				from={history?.from ?? Date.now() - selectedRange.ms}
				to={history?.to ?? Date.now()}
			/>
		</div>

		<ColorChart
			readings={history?.sensors?.opt4048 ?? []}
			latest={latest.opt4048?.latest ?? null}
			from={history?.from ?? Date.now() - selectedRange.ms}
			to={history?.to ?? Date.now()}
		/>

		<ThermalImage image={thermal} now={thermalNow} />
	</section>

	<section class="collection">
		<h2>Collection</h2>
		<table>
			<thead>
				<tr>
					<th>Sensor</th>
					<th>Interval</th>
					<th>Retained on device</th>
					<th>Stored here this run</th>
					<th>Newest reading</th>
				</tr>
			</thead>
			<tbody>
				{#each SENSORS as sensor}
					<tr>
						<td>{SENSOR_LABELS[sensor]}</td>
						<td>{latest[sensor]?.intervalMs ? `${latest[sensor].intervalMs / 1000} s` : '—'}</td>
						<td>{latest[sensor]?.deviceRetained ?? '—'}</td>
						<td>{latest[sensor]?.storedTotal ?? 0}</td>
						<td>{formatAge(latest[sensor]?.lastReadingAt)}</td>
					</tr>
				{/each}
			</tbody>
		</table>
		{#if status?.rebootsSeen}
			<p class="age">
				The device has restarted {status.rebootsSeen}
				{status.rebootsSeen === 1 ? 'time' : 'times'} since this server started; readings taken
				before each restart are kept.
			</p>
		{/if}
	</section>
</main>

<style>
	/* The window is as wide as it is; the dashboard fills it rather than
	   forcing the reader down a narrow column. Only a thin gutter is kept so
	   the cards and charts do not touch the edges of the screen. */
	main {
		width: 100%;
		margin: 0;
		padding: 0.9rem 0.9rem 1.25rem;
	}

	header {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: 1rem;
		margin-bottom: 0.9rem;
	}

	h1 {
		margin: 0;
		font-size: 1.25rem;
	}

	.subtitle {
		margin: 0.3rem 0 0;
		color: #94a3b8;
		font-size: 0.85rem;
	}

	code {
		color: #cbd5f5;
	}

	.connection {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		padding: 0.4rem 0.8rem;
		border: 1px solid #1f2937;
		border-radius: 999px;
		background: #111827;
		font-size: 0.8rem;
		color: #94a3b8;
	}

	.dot {
		width: 0.55rem;
		height: 0.55rem;
		border-radius: 50%;
		background: #22c55e;
	}

	.connection.offline .dot {
		background: #ef4444;
	}

	.notice {
		margin: 0 0 1.25rem;
		padding: 0.7rem 0.9rem;
		border: 1px solid #7f1d1d;
		border-radius: 8px;
		background: #1f1214;
		color: #fca5a5;
		font-size: 0.85rem;
	}

	.notice.subtle {
		border-color: #1f2937;
		background: #111827;
		color: #94a3b8;
	}

	.cards {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
		gap: 0.6rem;
		margin-bottom: 0.9rem;
	}

	.cards article {
		padding: 0.6rem 0.75rem;
		background: #111827;
		border: 1px solid #1f2937;
		border-radius: 10px;
	}

	.cards h2 {
		margin: 0;
		font-size: 0.78rem;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: #94a3b8;
	}

	.value {
		margin: 0.25rem 0 0.15rem;
		font-size: 1.4rem;
		font-variant-numeric: tabular-nums;
	}

	.value span {
		font-size: 0.9rem;
		color: #94a3b8;
	}

	.age {
		margin: 0;
		font-size: 0.75rem;
		color: #64748b;
	}

	.unqualified .value {
		color: #64748b;
	}

	.ranges {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.4rem;
		margin-bottom: 0.7rem;
	}

	.ranges button {
		padding: 0.35rem 0.75rem;
		border: 1px solid #1f2937;
		border-radius: 999px;
		background: #111827;
		color: #cbd5f5;
		font: inherit;
		font-size: 0.8rem;
		cursor: pointer;
	}

	.ranges button.selected {
		border-color: #4f9cf9;
		color: #e2e8f0;
	}

	.range-note {
		margin-left: auto;
		font-size: 0.75rem;
		color: #64748b;
	}

	/* As many chart columns as the window can hold at a width where the axis
	   labels still have room, so a wide screen shows every chart at once
	   instead of a single tall stack. */
	.charts {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(420px, 1fr));
		gap: 0.6rem;
	}

	.charts .wide {
		grid-column: 1 / -1;
	}

	.collection {
		margin-top: 1.1rem;
	}

	.collection h2 {
		font-size: 0.95rem;
		margin-bottom: 0.4rem;
	}

	table {
		width: 100%;
		border-collapse: collapse;
		font-size: 0.82rem;
	}

	th,
	td {
		padding: 0.45rem 0.6rem;
		text-align: left;
		border-bottom: 1px solid #1f2937;
	}

	th {
		color: #94a3b8;
		font-weight: 600;
	}

	@media (min-width: 1400px) {
		main {
			padding: 1rem 1.25rem 1.5rem;
		}
	}

	@media (max-width: 640px) {
		.charts {
			grid-template-columns: 1fr;
		}
	}
</style>
