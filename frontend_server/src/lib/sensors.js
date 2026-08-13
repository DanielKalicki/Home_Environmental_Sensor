/**
 * Definitions of what each sensor reports, shared by the server and the page.
 *
 * The field names are the ones the device uses; see `software/README.md` for
 * what each of them means. Grouping them here keeps the chart list and the
 * current-value cards from drifting apart.
 */

/** Sensors, in the order the dashboard shows them. */
export const SENSORS = ['scd41', 'sps30', 'bme690', 'as7343'];

/** Human-readable name of each sensor and what it measures. */
export const SENSOR_LABELS = {
	scd41: 'SCD41 — CO₂, temperature, humidity',
	sps30: 'SPS30 — particulate matter',
	bme690: 'BME690 — temperature, humidity, pressure, gas',
	as7343: 'AS7343 — visible-light spectrum'
};

/**
 * Centre wavelength, in nanometres, of each of the AS7343's filtered channels.
 * The reading field for one of them is `nm_<wavelength>`.
 */
export const SPECTRAL_WAVELENGTHS_NM = [405, 425, 450, 475, 515, 550, 555, 600, 640, 690, 745, 855];

/**
 * The charts on the dashboard.
 *
 * A chart draws one or more series; a series names the sensor it comes from
 * and the field of that sensor's readings it plots. Series from different
 * sensors can share a chart when they are the same quantity, which is how the
 * SCD41's measured CO₂ and the BME690's estimate end up side by side.
 */
export const CHARTS = [
	{
		id: 'co2',
		title: 'Carbon dioxide',
		unit: 'ppm',
		series: [
			{ sensor: 'scd41', field: 'co2_ppm', label: 'SCD41 (measured)', color: '#4f9cf9' },
			{
				sensor: 'bme690',
				field: 'co2_equivalent_ppm',
				label: 'BME690 (estimated)',
				color: '#9b8cf9'
			}
		]
	},
	{
		id: 'temperature',
		title: 'Temperature',
		unit: '°C',
		decimals: 2,
		series: [
			{ sensor: 'scd41', field: 'temperature_celsius', label: 'SCD41', color: '#f97b4f' },
			{ sensor: 'bme690', field: 'temperature_celsius', label: 'BME690', color: '#f9c74f' }
		]
	},
	{
		id: 'humidity',
		title: 'Relative humidity',
		unit: '%',
		decimals: 2,
		series: [
			{ sensor: 'scd41', field: 'humidity_percent', label: 'SCD41', color: '#4fd1c5' },
			{ sensor: 'bme690', field: 'humidity_percent', label: 'BME690', color: '#4f9cf9' }
		]
	},
	{
		id: 'pressure',
		title: 'Air pressure',
		unit: 'hPa',
		decimals: 2,
		series: [
			{
				sensor: 'bme690',
				field: 'pressure_pascals',
				label: 'BME690',
				color: '#a3e635',
				scale: 0.01
			}
		]
	},
	{
		id: 'particulates',
		title: 'Particulate matter',
		unit: 'µg/m³',
		decimals: 2,
		series: [
			{ sensor: 'sps30', field: 'pm1_0', label: 'PM1.0', color: '#4f9cf9' },
			{ sensor: 'sps30', field: 'pm2_5', label: 'PM2.5', color: '#4fd1c5' },
			{ sensor: 'sps30', field: 'pm4_0', label: 'PM4.0', color: '#f9c74f' },
			{ sensor: 'sps30', field: 'pm10', label: 'PM10', color: '#f97b4f' }
		]
	},
	{
		id: 'particle-size',
		title: 'Typical particle size',
		unit: 'µm',
		decimals: 3,
		series: [
			{ sensor: 'sps30', field: 'typical_particle_size', label: 'SPS30', color: '#c084fc' }
		]
	},
	{
		id: 'iaq',
		title: 'Indoor air quality index',
		unit: '',
		decimals: 1,
		series: [
			{ sensor: 'bme690', field: 'iaq', label: 'IAQ', color: '#4fd1c5' },
			{ sensor: 'bme690', field: 'static_iaq', label: 'Static IAQ', color: '#9b8cf9' }
		]
	},
	{
		id: 'tvoc',
		title: 'Volatile organic compounds',
		unit: 'ppb',
		decimals: 3,
		series: [
			{ sensor: 'bme690', field: 'tvoc_equivalent_ppb', label: 'TVOC (estimated)', color: '#f97b4f' }
		]
	},
	{
		id: 'gas',
		title: 'Gas sensor',
		unit: 'Ω',
		series: [
			{ sensor: 'bme690', field: 'gas_resistance_ohms', label: 'Resistance', color: '#f9c74f' }
		]
	},
	{
		id: 'light',
		title: 'Unfiltered light',
		unit: 'counts',
		series: [{ sensor: 'as7343', field: 'visible', label: 'Visible photodiode', color: '#fbbf24' }]
	}
];

/**
 * Every sensor field the charts read, per sensor.
 *
 * The history endpoint sends only these, so a request for a week of readings
 * does not also carry the fields nothing on the page draws.
 */
export function chartedFields(sensor) {
	const fields = new Set();
	for (const chart of CHARTS) {
		for (const series of chart.series) {
			if (series.sensor === sensor) {
				fields.add(series.field);
			}
		}
	}
	return [...fields];
}

/**
 * The value a series plots, taken from one reading.
 *
 * A field is normally a single number. The AS7343's `visible` and `flicker`
 * fields are three separate measurements of the same light, one per
 * integration cycle, and are plotted as their mean. `scale` converts a unit,
 * as for pressure, which the device reports in pascals and the chart shows in
 * hectopascals.
 */
export function seriesValue(reading, series) {
	const raw = reading?.[series.field];
	if (raw === null || raw === undefined) {
		return null;
	}

	let value;
	if (Array.isArray(raw)) {
		if (raw.length === 0) {
			return null;
		}
		value = raw.reduce((sum, entry) => sum + entry, 0) / raw.length;
	} else if (typeof raw === 'number') {
		value = raw;
	} else {
		return null;
	}

	if (!Number.isFinite(value)) {
		return null;
	}

	return series.scale ? value * series.scale : value;
}
