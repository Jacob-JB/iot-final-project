# Server setup: Telegraf → InfluxDB Cloud → Grafana Cloud

How to build the server side for the smart blind on the same InfluxDB Cloud and Grafana Cloud
free plans used in the week 9–10 labs. The MQTT topics and payloads are defined in
[`interface.md`](interface.md).

```
 ESP32 ──MQTT──▶ broker.emqx.io ──▶ Telegraf ──HTTPS──▶ InfluxDB Cloud ──▶ Grafana Cloud
   ▲                   │            (your laptop)        (bucket)           (dashboard)
   └──────── command ◀──────────── MQTTX or mosquitto_pub (you)
```

| Part | What it does | Where it runs |
|---|---|---|
| Firmware | Publishes temperature and light over MQTT and listens for position commands | ESP32 |
| MQTT broker | Passes messages between publishers and subscribers. Stores nothing except retained messages | `broker.emqx.io` (public) |
| **Telegraf** | Subscribes to the blind's topics, turns each JSON message into InfluxDB points, and writes them to the bucket | Your laptop |
| InfluxDB Cloud | Stores the time series | InfluxData |
| Grafana Cloud | Queries InfluxDB with Flux and draws the dashboard | Grafana Labs |

## What's different from the labs

In the labs, the ESP32 wrote straight into InfluxDB over HTTP. Here it doesn't, and that changes
a few things:

| In the labs | In this project | Why |
|---|---|---|
| ESP32 → InfluxDB directly (`InfluxDbClient`, `writePoint`) | ESP32 → MQTT broker → **Telegraf** → InfluxDB | InfluxDB doesn't speak MQTT, so something has to subscribe and forward. This is the "scalable architecture" from the week 10 lecture, slides 26–30 |
| URL, org, token and bucket compiled into the firmware | All of these live in `server/.env` on the laptop. The firmware knows only the broker | Loose coupling. You can change token or bucket without reflashing |
| Device syncs time over NTP and stamps each point | No timestamps from the device. Telegraf stamps each point when it arrives | Delay through the broker is well under the 2 s sample interval, so arrival time is accurate enough |
| You built line protocol (`addField("temp", …)`) | Telegraf builds it from the JSON. Every JSON key becomes a field and the topic gives the measurement and `device` tag | No firmware changes needed when you add a field |
| Data only goes device → cloud | Position commands go back over MQTT (`command`) | The server side can control the blind, not just watch it |
| Nothing to keep running except the ESP32 | **Telegraf must be running** on your laptop for data to reach InfluxDB | If the laptop sleeps, the dashboard stops updating. The blind keeps working on its own |

Everything from InfluxDB onwards is the same as the labs: bucket, token, Data Explorer, Grafana
data source with Flux, and panels.

---

## Already done in the repo

- [`server/telegraf.conf`](../server/telegraf.conf): MQTT input to InfluxDB Cloud output.
- [`server/run-telegraf.sh`](../server/run-telegraf.sh): loads `server/.env` and starts Telegraf.
- [`server/.env.example`](../server/.env.example): template for your credentials. `server/.env`
  is git-ignored.
- Telegraf (`brew install telegraf`) and the Mosquitto client tools (`brew install mosquitto`)
  installed on the Mac.
- Config checked with a dry run against the live broker. Example parsed output:

  ```
  telemetry,device=test00 light=238.6,temperature=22.83 1793794299121362000
  status,device=43ba10 online=0 1793794295341113000
  ```

  That's the same line protocol you printed over serial in the week 9 lab.

## 1. Create the bucket and token in InfluxDB Cloud

Log in at <https://cloud2.influxdata.com> with the account from the labs.

1. **Load Data → Buckets → Create Bucket**, name `smartblind`, retention 30 days.
   The free plan allows only **2 buckets**. If both are already used by lab buckets, delete one
   or reuse it. Then set `INFLUX_BUCKET` to that bucket's name.
2. **Load Data → API Tokens → Generate API Token.** Make one for Telegraf and a separate one for
   Grafana, as the week 9 slides suggest (one token per purpose).
   - *Custom API Token* with **write** on `smartblind` is enough for Telegraf, and **read** on
     `smartblind` is enough for Grafana. An *All Access* token, as in the labs, also works.
   - Copy each token straight away. InfluxDB only shows it once.
3. Note the **org ID** from the browser URL: `…/orgs/<org ID>/load-data/buckets`.

## 2. Fill in `server/.env`

```sh
cp server/.env.example server/.env
```

Then edit `server/.env`:

```sh
INFLUX_URL=https://us-east-1-1.aws.cloud2.influxdata.com   # same URL as the labs
INFLUX_ORG=<org ID from step 1>
INFLUX_BUCKET=smartblind
INFLUX_TOKEN=<Telegraf token>
```

## 3. Run Telegraf

Check that the blind is publishing (you should see a telemetry line every 2 s):

```sh
mosquitto_sub -h broker.emqx.io -t 'jacob-brendan/smartblind/#' -v
```

Do a dry run. This prints the points without writing them, much like the serial output in the
week 9 lab:

```sh
./server/run-telegraf.sh --test --test-wait 10
```

Then run it for real and leave the terminal open:

```sh
./server/run-telegraf.sh
```

Look for `Connected [tcp://broker.emqx.io:1883]` and no `E!` lines. Errors writing to InfluxDB
show up here every 5 s, so a wrong token or bucket is obvious straight away.

Then confirm the data arrived. In InfluxDB Cloud, open **Data Explorer**, choose bucket
`smartblind`, measurement `telemetry`, tick a field, and click **Run**. The SQL it generates
looks like this:

```sql
SELECT * FROM "telemetry" WHERE time >= now() - interval '1 hour'
```

Use `caffeinate -i ./server/run-telegraf.sh` during the demo so the Mac doesn't sleep.

## 4. Connect Grafana Cloud

This is the same as week 10, slides 18–20: **Connections → Data sources → Add new data source →
InfluxDB**.

| Setting | Value |
|---|---|
| Query language | **Flux** |
| URL | `https://us-east-1-1.aws.cloud2.influxdata.com` |
| Organization | Your org ID or org name |
| Token | The Grafana token from step 1 |
| Default bucket | `smartblind` |

Click **Save & test**. You should get a green tick and "buckets found". The slides also turn on
*Basic auth* with your InfluxDB login. The token is what actually authenticates for Flux, so if
the test fails with basic auth on, turn it off.

## 5. Build the dashboard

Create a new dashboard. Add a **`device` variable** so one dashboard works for any blind:
*Settings → Variables → New → Query*, name `device`, query:

```flux
from(bucket: "smartblind")
  |> range(start: -30d)
  |> filter(fn: (r) => r._measurement == "telemetry" and r._field == "temperature")
  |> keep(columns: ["device"])
  |> distinct(column: "device")
```

Every telemetry panel uses the same query shape, like the RSSI example on slide 23. Change only
the field list:

```flux
from(bucket: "smartblind")
  |> range(start: v.timeRangeStart, stop: v.timeRangeStop)
  |> filter(fn: (r) => r._measurement == "telemetry" and r.device == "${device}")
  |> filter(fn: (r) => r._field == "temperature" or r._field == "temperature_avg")
  |> aggregateWindow(every: v.windowPeriod, fn: mean, createEmpty: false)
```

Suggested panels:

| Panel | Type | Query | Tips |
|---|---|---|---|
| Temperature | Time series | Field `temperature` | Unit °C. Add threshold lines at 20 and 24 (`temp_low`/`temp_high` in `src/controller.rs`) so it's clear when the blind starts heating or cooling the room |
| Light | Time series | Field `light` | Unit lux. Use a **log** Y axis, since the controller maps light on a log scale. Threshold lines at 300 and 1200 (`lux_low`/`lux_high`) |
| Current readings | Stat | `temperature` and `light` with `last()` in place of `aggregateWindow(...)` | Two big numbers for the demo |
| Online | Stat | `status` measurement, field `online` | Use the status query below. Map 1 → Online (green) and 0 → Offline (red) |

**Status query.** The status is only published on connect and disconnect, so the latest point is
often older than the dashboard time range. Query a fixed long window and take the last value:

```flux
from(bucket: "smartblind")
  |> range(start: -30d)
  |> filter(fn: (r) => r._measurement == "status" and r.device == "${device}")
  |> last()
```

**Keep a copy of the dashboard.** Use *Export → Export as JSON* and save it to
`server/grafana-dashboard.json`, so you can re-import it if anything goes wrong. To share it for
marking, use *Share → Share snapshot → Publish*, as on slide 25.

## 6. Send commands (server → device)

Use MQTTX (`brew install --cask mqttx`, a GUI) or `mosquitto_pub`. Replace `<id>` with the
device ID, or use `all`:

```sh
P=jacob-brendan/smartblind/<id>
mosquitto_pub -h broker.emqx.io -t $P/command -m '{"position":0.4}'   # manual, 40 % open
mosquitto_pub -h broker.emqx.io -t $P/command -m '{"mode":"auto"}'    # back to automatic
```

The device logs `accepted command on …` or `rejected command on …: <reason>`.

## 7. Demo day

The dashboard shows the inputs, temperature and light. The servo shows the response. Walk
through each case:

1. **Dull room:** light below 300 lux, so the blind stays open.
2. **Phone torch on the photoresistor:** light goes past 1200 lux, so the blind closes to about
   0.7 open (`glare_shade`).
3. **Torch plus a finger on the thermistor:** once the temperature passes 24 °C, the blind closes
   further (hot and sunny).
4. **Cover the photoresistor:** light drops below 20 lux, which is night mode, so the blind
   closes (`night_position`).
5. **Manual override:** send `{"position":0.4}` and the servo moves there and holds. Then send
   `{"mode":"auto"}`.

Temperature is smoothed over 60 s, so step 3 takes a minute or so. To make it react faster for
the demo, lower `temp_smoothing_s` and `temp_high` in `BlindConfig::default()`
([`src/controller.rs`](../src/controller.rs)) and reflash.

**Report material from week 10:**
- **Write rate:** a telemetry message is about 40 bytes every 2 s, roughly 20 B/s. That's well
  under the free plan's 5 MB per 5 min (about 17 KB/s).
- **Protocol choice:** MQTT keeps per-message overhead low on the device. Telegraf batches the
  HTTP writes to InfluxDB (`flush_interval`), which is the batching benefit from slides 5–11.
- **Security:** `broker.emqx.io` is public, so anyone can read or publish to these topics. The fix
  would be a private broker with TLS and authentication.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Nothing in `mosquitto_sub` | Device side: Wi-Fi, or broker or prefix set over BLE. Check the firmware logs |
| Telegraf `401 Unauthorized` | Wrong token, or the token has no write access to the bucket |
| Telegraf `404 bucket not found` | `INFLUX_BUCKET` or `INFLUX_ORG` is wrong. Use the org ID from the URL |
| Data in InfluxDB but the Grafana panel is empty | The `device` variable is unset, or the bucket name is wrong. Check the query inspector |
| Online panel empty | The status point is older than the time range. Use the `range(start: -30d) \|> last()` query |
| Old device IDs in the `device` list | Retained messages from earlier firmware. Clear one with `mosquitto_pub -h broker.emqx.io -t <topic> -r -n` |
