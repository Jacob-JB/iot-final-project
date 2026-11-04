# Device interface

Each blind reads its thermistor and photoresistor every 2 s, filters them, decides how open the
blind should be, and drives the servo there. It works on its own without a network. MQTT lets
you watch the readings and override the position. BLE sets up Wi-Fi and MQTT.

Positions are **openness**: `0` is fully closed and `1` is fully open. If the blind
closes when it should open, set `invert` to `true` in the config.

## Control algorithm

Source: [`src/controller.rs`](../src/controller.rs). Unit tests: run `cargo test` in
[`host-tests/`](../host-tests/Cargo.toml).

1. **Filtering.** Each sensor goes through a median of the last 3 samples, which removes single
   spikes such as a reflection or noise. It then goes through an exponential moving average with
   time constant `temp_smoothing_s` / `light_smoothing_s`. The EMA weight is `1 − e^(−Δt/τ)`, so
   it responds at the same rate however often samples arrive. Implausible temperatures (outside
   −30 to 90 °C, for example a disconnected divider) are dropped and reported as `sensor_fault`.
2. **Demands**, each from 0 to 1:
   - `sun`: smoothed light mapped between `lux_low` and `lux_high` on a log scale.
   - `cooling`: how far the smoothed temperature is above `temp_high`, reaching 1 at
     `temp_high + temp_band`.
   - `heating`: how far it is below `temp_low`, reaching 1 at `temp_low − temp_band`.
3. **Target.**
   `shade = glare_shade + (1 − glare_shade)·cooling − glare_shade·heating`, then
   `target = 1 − sun·shade`. In practice:
   - Hot and sunny: closes, blocking solar gain to cut cooling load.
   - Cold and sunny: opens fully for free solar heating.
   - Comfortable and sunny: closes by `glare_shade` against glare.
   - Dull day: stays open for daylight.
   - At night (light below `night_lux`, ending at twice that to avoid flicker), the blind goes to
     `night_position`.
4. **Actuation.** The setpoint only follows the target when it changes by at least `deadband`,
   or reaches fully open or closed, so the servo doesn't twitch. The servo then slews there at
   a speed that would cover the full range in `travel_s`.
5. **Manual override.** A manual command holds a position until `{"mode":"auto"}`, or until
   `manual_timeout_s` has passed if it is not 0.

## MQTT

The defaults are broker `broker.emqx.io:1883` and topic prefix `jacob-brendan/smartblind`. Both
can be changed over BLE. `<id>` is the device ID: by default the last 3 bytes of the MAC address
in hex, also settable over BLE. Using `all` in place of `<id>` on the two inbound topics
addresses every device.

| Topic | Direction | Retained | Payload |
|---|---|---|---|
| `<prefix>/<id>/telemetry` | device → server | no | `{"temperature":22.83,"light":238.6}`, °C and raw lux, every 2 s |
| `<prefix>/<id>/status` | device → server | yes | `{"online":1}` on connect. The broker sends `{"online":0}` when the device drops off (30 s keep-alive) |
| `<prefix>/<id>/command` | server → device | — | Command JSON |

Every payload is flat JSON with numeric values, so Telegraf's `json` parser maps it straight onto
InfluxDB fields.

### Commands

| Payload | Effect |
|---|---|
| `{"position":0.4}` | Manual mode, 40 % open |
| `{"mode":"manual"}` | Manual mode, holding the current position |
| `{"mode":"auto"}` | Back to automatic control, jumping straight to the target |

### Config

The controller uses the defaults in `BlindConfig::default()` in
[`src/controller.rs`](../src/controller.rs). Change them there and reflash to tune it.

| Field | Default | Meaning |
|---|---|---|
| `temp_low` | 20 | °C below which the blind lets sun in to heat the room |
| `temp_high` | 24 | °C above which the blind blocks sun to cool the room |
| `temp_band` | 2 | °C past a limit for the full response. Smaller is more aggressive |
| `lux_low` | 300 | lux at and below which there is no sun to block |
| `lux_high` | 1200 | lux at and above which the sun counts as full strength |
| `night_lux` | 20 | lux below which it is night. `0` disables night mode |
| `night_position` | 0 | Openness at night |
| `glare_shade` | 0.3 | How far to close against full sun at a comfortable temperature |
| `temp_smoothing_s` | 60 | EMA time constant for temperature. `0` disables smoothing |
| `light_smoothing_s` | 30 | EMA time constant for light. `0` disables smoothing |
| `deadband` | 0.05 | Smallest target change that moves the blind |
| `travel_s` | 10 | Seconds for a full close-to-open move. `0` is instant |
| `manual_timeout_s` | 0 | Seconds before a manual override reverts to auto. `0` never reverts |
| `invert` | false | Swaps which servo end is open |

The defaults suit a desk demo: a phone torch on the photoresistor reads as full sun.

With the 10 kΩ fixed resistor, the photoresistor divider saturates the ADC at about **1500 lux**,
so readings stop rising there and `lux_high` must stay below it. For a real window (direct sun
through glass is tens of thousands of lux), use a smaller fixed resistor such as 1 kΩ, change
`DIVIDER_OHMS` in `src/sensors.rs` to match, raise `lux_low`/`lux_high`, and lengthen the
smoothing.

### Telegraf starting point

```toml
[[inputs.mqtt_consumer]]
  servers = ["tcp://broker.emqx.io:1883"]
  topics = [
    "jacob-brendan/smartblind/+/telemetry",
    "jacob-brendan/smartblind/+/status",
  ]
  data_format = "json"

  # Measurement from the last topic level, device tag from the ID level.
  [[inputs.mqtt_consumer.topic_parsing]]
    topic = "jacob-brendan/smartblind/+/+"
    measurement = "_/_/_/measurement"
    tags = "_/_/device/_"
```

This gives the measurements `telemetry` and `status`, each tagged with `device`.

## BLE

Press the BOOT button to start advertising as `smart-blind`. Advertising stops after 30 s without
a connection, or when the button is pressed again. The LED blinks blue while advertising and
purple while connected.

| Service | Characteristic | Access | Value |
|---|---|---|---|
| Wi-Fi `…4c00` | SSID `…4c01` | read, write | UTF-8 string. Writing an empty one forgets the network |
| | Password `…4c02` | write | UTF-8 string. Writing it connects |
| MQTT `…4d00` | Broker host `…4d01`, port `…4d02` (u16 LE), topic prefix `…4d03`, device ID `…4d04` | read, write | |

UUIDs are `5c0b1a10-6f2e-4b8e-9d3a-7e1f2a3b` followed by the 4 digits shown. Invalid writes are
rejected with an ATT error.
