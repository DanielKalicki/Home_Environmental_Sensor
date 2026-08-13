# Frontend server

A small server that follows the sensor device over the network, keeps its own
copy of every reading, and serves a dashboard drawn from that copy.

It is separate from the web page the device itself serves. The device holds one
day of readings in PSRAM and overwrites the oldest as it goes, and it loses all
of them when it restarts; this server appends what it pulls to files on disk, so
the history it can draw is as long as it has been running. It also means a
browser never has to talk to the device: the device answers one connection at a
time, and here that one connection is used by a single poller rather than by
every open tab.

The device is not modified in any way. Everything below is built on the two
read-only endpoints it already serves, `GET /api/status` and
`GET /api/readings`, which are documented in
[software/README.md](../software/README.md).

## Running it

Node.js 18 or newer is required.

```sh
cd frontend_server
npm install
cp .env.example .env      # then set DEVICE_URL to the device's address
npm run dev               # development server on http://localhost:5173
```

For a long-running instance, build once and start the compiled server:

```sh
npm run build
DEVICE_URL=http://192.168.1.50 PORT=3000 npm start
```

The device prints the address it got from DHCP at boot as a line starting with
`Web server:`; `tools/serial.sh` shows it.

### Settings

All of them are environment variables, read once at startup, and all have a
default, so only `DEVICE_URL` normally has to be set. See `.env.example`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `DEVICE_URL` | `http://192.168.1.50` | Base URL of the device. |
| `POLL_INTERVAL_MS` | `5000` | Milliseconds between two `/api/status` polls of the device. |
| `REQUEST_TIMEOUT_MS` | `15000` | How long to wait for one device response before giving up on it. |
| `DATA_DIR` | `data` | Where the collected readings are written. |
| `PORT` | `3000` | Port this server listens on. Only affects `npm start`. |

## How collection works

The poller starts with the server process, not with the first browser request,
so nothing is missed while nobody is looking at the dashboard.

Every pass it asks the device for `/api/status`, which reports each sensor's
`first_sequence` (its oldest retained reading) and `next_sequence` (the number
the next reading will get). It keeps a cursor per sensor and requests exactly
the readings between the cursor and `next_sequence` with `/api/readings`, in
pages of 2000, which is the device's maximum. On the first pass the cursor
starts at `first_sequence`, so the device's whole retained day is taken at once.
Afterwards each pass fetches the handful of readings taken since the last one,
usually none or one per sensor.

**Dating the readings.** The device has no real-time clock. It dates a reading
by the uptime at which it was taken and reports its own uptime in the response,
so the difference between the two is how long before that response the reading
was taken. Subtracting that from the local time the response arrived puts the
reading on a wall-clock timeline. Every reading of a page is dated against that
one response, so readings keep their exact spacing relative to each other and
only the network round trip, tens of milliseconds, is added to all of them
alike.

**Device restarts.** A `uptime_ms` lower than the one seen last pass means the
device rebooted and its sequence numbers restarted at zero. The cursors are
dropped and every sensor is picked up from its oldest retained reading again.
The device then serves a day of readings that are mostly already stored here,
so the overlap is dropped on append: a reading is written only if it is newer
than the newest already stored for that sensor. Readings the device took while
this server was down are recovered this way, as far back as the device still
retains them.

**When the device is unreachable.** The pass fails, the failure is recorded and
shown on the dashboard, and the next pass tries again. Nothing already stored is
affected, and the cursors are untouched, so collection resumes where it stopped.

## How readings are stored

One file per sensor per calendar day (UTC), holding one JSON object per line:

```
data/scd41/2026-08-13.jsonl
data/sps30/2026-08-13.jsonl
```

Appending is a single write that does not rewrite what is already there, and a
query only opens the files whose day overlaps the range asked for, so neither
cost grows with the size of the history. The files are plain text and can be
read with any tool:

```sh
tail -1 data/scd41/2026-08-13.jsonl
```

Each object is the reading exactly as the device reported it, minus
`taken_at_ms`, plus two fields: `t`, the wall-clock time it was taken in
milliseconds since the Unix epoch, and `device_uptime_ms`, the device uptime it
was originally dated by. The device's sequence numbers are not stored, because
they restart at zero on every reboot and so cannot order a history that spans
one.

Nothing is ever deleted. To drop old readings, delete the daily files.

## This server's own API

The dashboard uses only these; neither of them touches the device.

### `GET /api/status`

What the poller is doing and the newest reading collected from each sensor.
Answered from memory. Notable fields: `deviceOnline` (whether the last pass
reached the device), `lastError`, `deviceUptimeMs`, `rebootsSeen`,
`oldestReadingAt`, and per sensor `intervalMs`, `deviceRetained`, `storedTotal`
(readings written since this process started), `lastReadingAt` and `latest`.

### `GET /api/history`

Stored readings over a time range, ready to draw.

| Parameter | Default | Meaning |
| --- | --- | --- |
| `from` | `to` minus one hour | Start of the range, in milliseconds since the Unix epoch. |
| `to` | now | End of the range, the same way. |
| `points` | `600` | Largest number of points returned per sensor. Capped at 5000. |
| `sensor` | every sensor | Restrict the answer to `scd41`, `sps30`, `bme690` or `as7343`. |

A month of readings is far more than a chart a few hundred pixels wide can
show, so a range holding more readings than `points` is averaged into that many
equal buckets. A bucket's value is the mean of the readings in it rather than
one reading picked out of it, so a spike between two sampled points cannot
vanish entirely, and a bucket with no readings produces no point, so a gap in
the history stays a gap in the chart. Only the fields the charts draw are sent.

```sh
curl 'http://localhost:3000/api/status'
curl 'http://localhost:3000/api/history?sensor=scd41&points=100'
```

## The dashboard

Current values as cards, then one chart per quantity over a range chosen from
15 minutes to 30 days. The cards refresh every 5 seconds and the charts every
20 seconds. Quantities two sensors report are drawn together: the SCD41's
measured CO₂ against the BME690's estimate of it, and the two independent
temperature and humidity readings.

The BME690's gas-derived values — the air quality index, the CO₂ equivalent and
TVOC — are estimates against a baseline the sensor learns over time, and are
fixed placeholders until it has one. They are shown greyed out while
`iaq_accuracy` is 0, with a line saying so, rather than presented as
measurements.

The AS7343 panel shows the newest reading's twelve filtered channels as bars,
tinted with roughly the colour of their wavelength. The bars are raw counts:
they are not corrected for how differently the channels respond and are not
divided by the gain, so one bar standing above its neighbour does not by itself
mean there is more light at that wavelength. What a bar shows reliably is how
that one channel changes. The unfiltered photodiode is charted over time
alongside the other quantities.

## Layout

```
src/
  hooks.server.js           starts the poller when the server process starts
  lib/
    sensors.js              which fields exist and which charts draw them
    components/             the two chart components, plain SVG, no dependencies
    server/
      config.js             settings read from the environment
      device.js             client for the device's two endpoints
      poller.js             the collection loop
      store.js              the daily files on disk
  routes/
    +page.svelte            the dashboard
    api/status/             this server's status endpoint
    api/history/            this server's history endpoint
```
