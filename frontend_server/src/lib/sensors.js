/**
 * Definitions of what each sensor reports, shared by the server and the page.
 *
 * The field names are the ones the device uses; see `software/README.md` for
 * what each of them means. Grouping them here keeps the chart list and the
 * current-value cards from drifting apart.
 */

/** Sensors, in the order the dashboard shows them. */
export const SENSORS = ['scd41', 'sps30', 'bme690', 'as7343', 'bmp581', 'opt4048', 'sht41'];

/** Human-readable name of each sensor and what it measures. */
export const SENSOR_LABELS = {
	scd41: 'SCD41 — CO₂, temperature, humidity',
	sps30: 'SPS30 — particulate matter',
	bme690: 'BME690 — temperature, humidity, pressure, gas',
	as7343: 'AS7343 — visible-light spectrum',
	bmp581: 'BMP581 — pressure, temperature',
	opt4048: 'OPT4048 — illuminance and colour',
	sht41: 'SHT41 — temperature, humidity'
};

/**
 * Centre wavelength, in nanometres, of each of the AS7343's filtered channels.
 * The reading field for one of them is `nm_<wavelength>`.
 */
export const SPECTRAL_WAVELENGTHS_NM = [405, 425, 450, 475, 515, 550, 555, 600, 640, 690, 745, 855];

/**
 * The AS7343 fields the spectrum chart draws over time.
 *
 * The twelve channel counts, the gain they were measured at, and the twonalyse 
 * saturation flags. The gain is needed because the device changes it as the
 * light changes: two readings taken at different gains are not comparable
 * until each is divided by its own, so a chart that spans a gain change has to
 * be told what the gain was. The flags mark the readings whose counts were cut
 * off at the top of the converter's range and are therefore a floor rather
 * than a measurement.
 */
export const SPECTRAL_FIELDS = [
	...SPECTRAL_WAVELENGTHS_NM.map((nm) => `nm_${nm}`),
	'gain',
	'analog_saturation',
	'digital_saturation'
];

/**
 * The OPT4048 fields the chromaticity chart draws, on top of the ones its
 * charts already ask for.
 *
 * `cie_x` and `cie_y` are where the measured light falls on the CIE 1931
 * chromaticity diagram, which is the colour of the light with its brightness
 * divided out; the brightness is `lux`, and the two together are the whole of
 * what the sensor reports about the light. `overload` marks the readings whose
 * channels were cut off at the top of the converter's range, whose colour is
 * therefore not a measurement.
 *
 * A word on what happens to these over a long range: the history endpoint
 * averages readings into buckets, and the mean of several chromaticities is
 * not exactly the chromaticity of their combined light, which would need the
 * tristimulus values averaged and renormalised instead. Over a bucket of light
 * that did not change much the two are close, and over one that changed a lot
 * neither is a description of any single moment, so the plain mean is used.
 */
export const COLOUR_FIELDS = ['cie_x', 'cie_y', 'overload'];

/**
 * Values a field can plausibly take, for the fields where a stored reading
 * outside the range is known to be an artefact rather than a measurement.
 *
 * Only `cct_kelvin` needs this. Its colour temperature comes from a cubic
 * fitted along the black-body curve, and light far off that curve — which is
 * what the few counts of noise a dark room produces look like — sends the
 * cubic running away, to hundreds of millions of kelvin or to negative
 * numbers. The device no longer reports those, but readings recorded before it
 * stopped are still on disk, and one of them in a bucket is enough to drag the
 * bucket's mean and the chart's axis with it. Dropping them on the way out
 * keeps that history readable without rewriting the stored files.
 *
 * A field not listed here is not range-checked.
 */
export const FIELD_LIMITS = {
	cct_kelvin: { min: 1000, max: 25000 }
};

/**
 * Whether one value of `field` is a measurement rather than a known artefact.
 *
 * @param {string} field
 * @param {number} value
 */
export function withinFieldLimits(field, value) {
	const limits = FIELD_LIMITS[field];
	if (!limits) {
		return true;
	}
	return value >= limits.min && value <= limits.max;
}

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
			{ sensor: 'bme690', field: 'temperature_celsius', label: 'BME690', color: '#f9c74f' },
			{ sensor: 'bmp581', field: 'temperature_celsius', label: 'BMP581', color: '#f472b6' },
			{ sensor: 'sht41', field: 'temperature_celsius', label: 'SHT41', color: '#34d399' }
		]
	},
	{
		id: 'humidity',
		title: 'Relative humidity',
		unit: '%',
		decimals: 2,
		series: [
			{ sensor: 'scd41', field: 'humidity_percent', label: 'SCD41', color: '#4fd1c5' },
			{ sensor: 'bme690', field: 'humidity_percent', label: 'BME690', color: '#4f9cf9' },
			{ sensor: 'sht41', field: 'humidity_percent', label: 'SHT41', color: '#c084fc' }
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
			},
			{
				sensor: 'bmp581',
				field: 'pressure_pascals',
				label: 'BMP581',
				color: '#38bdf8',
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
	},
	{
		id: 'illuminance',
		title: 'Illuminance',
		unit: 'lux',
		decimals: 2,
		series: [{ sensor: 'opt4048', field: 'lux', label: 'OPT4048', color: '#facc15' }]
	},
	{
		// The colour temperature of light too far off the black-body curve for
		// the approximation to mean anything arrives as `null`, and a null is
		// left out of the line rather than drawn as a zero.
		id: 'colour-temperature',
		title: 'Correlated colour temperature',
		unit: 'K',
		decimals: 0,
		series: [{ sensor: 'opt4048', field: 'cct_kelvin', label: 'OPT4048', color: '#fb923c' }]
	}
];

/**
 * Every sensor field the charts read, per sensor.
 *
 * The history endpoint sends only these, so a request for a week of readings
 * does not also carry the fields nothing on the page draws. The AS7343's
 * spectral channels and the OPT4048's colour coordinates are added on top of
 * what `CHARTS` lists, because the spectrum chart and the chromaticity chart
 * draw them themselves rather than as a series.
 */
export function chartedFields(sensor) {
	const extra = { as7343: SPECTRAL_FIELDS, opt4048: COLOUR_FIELDS }[sensor] ?? [];
	const fields = new Set(extra);
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
