//! The blind control algorithm, kept free of hardware so it can be tested on the host.
//!
//! Positions are openness, where 0 is fully closed and 1 is fully open.

use serde::{Deserialize, Serialize};

/// Factor above [`BlindConfig::night_lux`] the smoothed light must rise to before night ends, so
/// the blind doesn't toggle when the light hovers around the threshold.
const NIGHT_HYSTERESIS: f32 = 2.0;

/// Temperatures outside this range in °C are treated as a sensor fault, such as a disconnected
/// thermistor or divider resistor.
const PLAUSIBLE_TEMPERATURE: core::ops::RangeInclusive<f32> = -30.0..=90.0;

/// Settings for the control algorithm. Every field can be changed remotely.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlindConfig {
    /// Below this room temperature in °C the blind lets sun in to warm the room.
    pub temp_low: f32,
    /// Above this room temperature in °C the blind blocks sun to cool the room.
    pub temp_high: f32,
    /// How many °C past [`Self::temp_low`] or [`Self::temp_high`] it takes to reach the full
    /// heating or cooling response. Smaller is more aggressive.
    pub temp_band: f32,
    /// Light in lux at and below which there is no sun to block.
    pub lux_low: f32,
    /// Light in lux at and above which the sun counts as full strength.
    pub lux_high: f32,
    /// Light in lux below which it counts as night. 0 disables night mode.
    pub night_lux: f32,
    /// Openness held at night, for privacy and insulation.
    pub night_position: f32,
    /// How far the blind closes against full sun while the temperature is comfortable, to cut
    /// glare.
    pub glare_shade: f32,
    /// Time constant in seconds of the temperature moving average. 0 disables smoothing.
    pub temp_smoothing_s: f32,
    /// Time constant in seconds of the light moving average. 0 disables smoothing.
    pub light_smoothing_s: f32,
    /// Smallest change in the automatic target that moves the blind.
    pub deadband: f32,
    /// Seconds the servo takes to travel from fully closed to fully open. 0 moves instantly.
    pub travel_s: f32,
    /// Seconds after a manual command before automatic control resumes. 0 stays manual until
    /// told otherwise.
    pub manual_timeout_s: f32,
    /// Swaps which servo end is open.
    pub invert: bool,
}

impl Default for BlindConfig {
    fn default() -> Self {
        Self {
            temp_low: 20.0,
            temp_high: 24.0,
            temp_band: 2.0,
            lux_low: 300.0,
            // The 10 kΩ divider saturates the ADC at about 1500 lux, so full sun must be below that.
            lux_high: 1200.0,
            night_lux: 20.0,
            night_position: 0.0,
            glare_shade: 0.3,
            temp_smoothing_s: 60.0,
            light_smoothing_s: 30.0,
            deadband: 0.05,
            travel_s: 10.0,
            manual_timeout_s: 0.0,
            invert: false,
        }
    }
}

/// A change to some fields of a [`BlindConfig`]. Unknown fields are rejected so typos aren't
/// silently ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlindConfigUpdate {
    pub temp_low: Option<f32>,
    pub temp_high: Option<f32>,
    pub temp_band: Option<f32>,
    pub lux_low: Option<f32>,
    pub lux_high: Option<f32>,
    pub night_lux: Option<f32>,
    pub night_position: Option<f32>,
    pub glare_shade: Option<f32>,
    pub temp_smoothing_s: Option<f32>,
    pub light_smoothing_s: Option<f32>,
    pub deadband: Option<f32>,
    pub travel_s: Option<f32>,
    pub manual_timeout_s: Option<f32>,
    pub invert: Option<bool>,
    /// Resets every field to its default before applying the others.
    #[serde(default)]
    pub reset: bool,
}

impl BlindConfig {
    /// Returns this config with `update` applied, or why the result would be invalid.
    pub fn updated(&self, update: &BlindConfigUpdate) -> Result<Self, &'static str> {
        let mut config = if update.reset {
            Self::default()
        } else {
            *self
        };

        macro_rules! apply {
            ($($field:ident),*) => {
                $(if let Some(value) = update.$field {
                    config.$field = value;
                })*
            };
        }

        apply!(
            temp_low,
            temp_high,
            temp_band,
            lux_low,
            lux_high,
            night_lux,
            night_position,
            glare_shade,
            temp_smoothing_s,
            light_smoothing_s,
            deadband,
            travel_s,
            manual_timeout_s,
            invert
        );

        config.validate().map(|()| config)
    }

    fn validate(&self) -> Result<(), &'static str> {
        let finite = [
            self.temp_low,
            self.temp_high,
            self.temp_band,
            self.lux_low,
            self.lux_high,
            self.night_lux,
            self.night_position,
            self.glare_shade,
            self.temp_smoothing_s,
            self.light_smoothing_s,
            self.deadband,
            self.travel_s,
            self.manual_timeout_s,
        ];
        let fraction = 0.0..=1.0;

        if !finite.iter().all(|value| value.is_finite()) {
            Err("all values must be finite")
        } else if self.temp_low >= self.temp_high {
            Err("temp_low must be below temp_high")
        } else if self.temp_band <= 0.0 {
            Err("temp_band must be positive")
        } else if self.lux_low <= 0.0 || self.lux_low >= self.lux_high {
            Err("lux_low must be positive and below lux_high")
        } else if self.night_lux < 0.0 || self.night_lux >= self.lux_low {
            Err("night_lux must be at least 0 and below lux_low")
        } else if !fraction.contains(&self.night_position)
            || !fraction.contains(&self.glare_shade)
        {
            Err("night_position and glare_shade must be between 0 and 1")
        } else if !(0.0..=0.5).contains(&self.deadband) {
            Err("deadband must be between 0 and 0.5")
        } else if self.temp_smoothing_s < 0.0
            || self.light_smoothing_s < 0.0
            || self.travel_s < 0.0
            || self.manual_timeout_s < 0.0
        {
            Err("times must not be negative")
        } else {
            Ok(())
        }
    }
}

/// The median of the last three samples, which removes single-sample spikes such as a
/// reflection or electrical noise while passing real changes through one sample later.
#[derive(Debug, Clone, Copy, Default)]
pub struct Median3 {
    samples: [f32; 3],
    len: usize,
}

impl Median3 {
    pub fn update(&mut self, sample: f32) -> f32 {
        self.samples.rotate_right(1);
        self.samples[0] = sample;
        self.len = (self.len + 1).min(3);

        let [a, b, c] = self.samples;
        match self.len {
            1 => a,
            2 => (a + b) / 2.0,
            _ => a.max(b).min(a.min(b).max(c)),
        }
    }
}

/// An exponential moving average for samples taken at irregular intervals.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ema {
    value: Option<f32>,
    last_ms: u64,
}

impl Ema {
    /// Adds `sample` taken at `now_ms` with time constant `tau_s`.
    ///
    /// The weight of the new sample is `1 - e^(-dt/tau)`, where `dt` is the time since the
    /// previous sample, so the average responds at the same rate however often it is sampled.
    /// The first sample is taken as is.
    pub fn update(&mut self, sample: f32, now_ms: u64, tau_s: f32) -> f32 {
        let value = match self.value {
            Some(value) if tau_s > 0.0 => {
                let dt_s = now_ms.saturating_sub(self.last_ms) as f32 / 1000.0;
                let alpha = 1.0 - libm::expf(-dt_s / tau_s);
                value + alpha * (sample - value)
            }
            _ => sample,
        };

        self.value = Some(value);
        self.last_ms = now_ms;
        value
    }

    pub fn value(&self) -> Option<f32> {
        self.value
    }
}

/// Who decides the blind position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The controller follows the sensors.
    Auto,
    /// The position is held where it was last set.
    Manual,
}

/// One raw sensor sample.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// °C, possibly not finite.
    pub temperature: f32,
    /// lux, possibly not finite.
    pub light: f32,
}

/// The controller state after a step, for telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct ControlState {
    /// Smoothed temperature in °C.
    pub temperature_avg: f32,
    /// Smoothed light in lux.
    pub light_avg: f32,
    /// Sun strength from 0 to 1, from the smoothed light.
    pub sun: f32,
    /// Cooling demand from 0 to 1, from the smoothed temperature.
    pub cooling: f32,
    /// Heating demand from 0 to 1, from the smoothed temperature.
    pub heating: f32,
    /// Openness the automatic control wants, even while in manual mode.
    pub target: f32,
    /// Openness the blind is being driven to.
    pub setpoint: f32,
    /// 1 in manual mode, otherwise 0.
    pub manual: u8,
    /// 1 at night, otherwise 0.
    pub night: u8,
    /// 1 if the last temperature sample was implausible, otherwise 0.
    pub sensor_fault: u8,
}

/// Turns sensor samples into a blind position.
#[derive(Debug, Default)]
pub struct Controller {
    temperature_median: Median3,
    light_median: Median3,
    temperature: Ema,
    light: Ema,
    night: bool,
    /// `None` until the first plausible sample.
    setpoint: Option<f32>,
    /// Time in ms of the manual command, if in manual mode.
    manual_since_ms: Option<u64>,
    state: ControlState,
}

impl Controller {
    /// Adds `sample` taken at `now_ms` and returns the setpoint to drive the blind to, or `None`
    /// if there is none yet.
    pub fn step(&mut self, config: &BlindConfig, sample: Sample, now_ms: u64) -> Option<f32> {
        self.expire_manual(config, now_ms);

        let temperature_ok = sample.temperature.is_finite()
            && PLAUSIBLE_TEMPERATURE.contains(&sample.temperature);
        let light_ok = sample.light.is_finite() && sample.light >= 0.0;
        self.state.sensor_fault = u8::from(!temperature_ok);

        // A faulty sensor holds its last average rather than steering the blind with garbage.
        if temperature_ok {
            let temperature = self.temperature_median.update(sample.temperature);
            self.temperature
                .update(temperature, now_ms, config.temp_smoothing_s);
        }
        if light_ok {
            let light = self.light_median.update(sample.light);
            self.light.update(light, now_ms, config.light_smoothing_s);
        }

        let (Some(temperature), Some(light)) = (self.temperature.value(), self.light.value())
        else {
            return self.setpoint;
        };

        self.night = if self.night {
            light < config.night_lux * NIGHT_HYSTERESIS
        } else {
            light < config.night_lux
        };

        let sun = sun_strength(config, light);
        let cooling = clamp01((temperature - config.temp_high) / config.temp_band);
        let heating = clamp01((config.temp_low - temperature) / config.temp_band);
        let target = if self.night {
            config.night_position
        } else {
            target_openness(config, sun, cooling, heating)
        };

        if self.manual_since_ms.is_none() {
            self.setpoint = Some(match self.setpoint {
                Some(setpoint) if !should_move(config, setpoint, target) => setpoint,
                _ => target,
            });
        }

        self.state = ControlState {
            temperature_avg: temperature,
            light_avg: light,
            sun,
            cooling,
            heating,
            target,
            setpoint: self.setpoint.unwrap_or(target),
            manual: u8::from(self.manual_since_ms.is_some()),
            night: u8::from(self.night),
            sensor_fault: self.state.sensor_fault,
        };

        self.setpoint
    }

    /// Switches to manual mode at `position`, or at the current setpoint if `None`. Returns the
    /// new setpoint.
    pub fn set_manual(&mut self, position: Option<f32>, now_ms: u64) -> Option<f32> {
        self.manual_since_ms = Some(now_ms);
        if let Some(position) = position {
            self.setpoint = Some(clamp01(position));
        }
        self.state.manual = 1;
        if let Some(setpoint) = self.setpoint {
            self.state.setpoint = setpoint;
        }

        self.setpoint
    }

    /// Switches to automatic mode, jumping straight to the automatic target. Returns the new
    /// setpoint.
    pub fn set_auto(&mut self) -> Option<f32> {
        if self.manual_since_ms.take().is_some() && self.temperature.value().is_some() {
            self.setpoint = Some(self.state.target);
            self.state.setpoint = self.state.target;
        }
        self.state.manual = 0;

        self.setpoint
    }

    /// Returns the openness the blind is being driven to, or `None` before the first plausible
    /// sample or manual position.
    pub fn setpoint(&self) -> Option<f32> {
        self.setpoint
    }

    pub fn mode(&self) -> Mode {
        match self.manual_since_ms {
            Some(_) => Mode::Manual,
            None => Mode::Auto,
        }
    }

    pub fn state(&self) -> &ControlState {
        &self.state
    }

    fn expire_manual(&mut self, config: &BlindConfig, now_ms: u64) {
        let timeout_ms = (config.manual_timeout_s * 1000.0) as u64;

        if let Some(since) = self.manual_since_ms
            && timeout_ms > 0
            && now_ms.saturating_sub(since) >= timeout_ms
        {
            self.manual_since_ms = None;
        }
    }
}

/// Maps light onto 0 to 1 between [`BlindConfig::lux_low`] and [`BlindConfig::lux_high`] on a
/// log scale, since perceived brightness and solar gain both span orders of magnitude.
fn sun_strength(config: &BlindConfig, light: f32) -> f32 {
    if light <= config.lux_low {
        return 0.0;
    }

    clamp01(libm::logf(light / config.lux_low) / libm::logf(config.lux_high / config.lux_low))
}

/// How open the blind should be in daytime.
///
/// The shade factor is how much of the sun to block: [`BlindConfig::glare_shade`] when the
/// temperature is comfortable, rising to 1 with cooling demand and falling to 0 with heating
/// demand. The blind then closes by that much of the current sun strength, so it stays open on
/// dull days whatever the temperature.
fn target_openness(config: &BlindConfig, sun: f32, cooling: f32, heating: f32) -> f32 {
    let shade = config.glare_shade + (1.0 - config.glare_shade) * cooling
        - config.glare_shade * heating;

    clamp01(1.0 - sun * clamp01(shade))
}

/// Whether the setpoint should follow `target`, ignoring changes inside the deadband unless they
/// reach a fully open or closed end.
fn should_move(config: &BlindConfig, setpoint: f32, target: f32) -> bool {
    let change = libm::fabsf(target - setpoint);
    let to_end = (target == 0.0 || target == 1.0) && change > 0.0;

    change >= config.deadband || to_end
}

fn clamp01(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

/// Run with `cargo test` in `host-tests/`. Skipped on the device target, which has no test
/// harness.
#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    fn sample(temperature: f32, light: f32) -> Sample {
        Sample { temperature, light }
    }

    /// Steps `count` more times with the same sample, 5 s apart.
    fn settle(controller: &mut Controller, config: &BlindConfig, s: Sample, count: u64) -> f32 {
        let start = controller.light.last_ms;
        let mut setpoint = None;
        for i in 1..=count {
            setpoint = controller.step(config, s, start + i * 5000);
        }
        setpoint.unwrap()
    }

    #[test]
    fn median_removes_a_single_spike() {
        let mut median = Median3::default();
        median.update(1.0);
        median.update(1.0);
        assert_eq!(median.update(100.0), 1.0);
        assert_eq!(median.update(1.0), 1.0);
    }

    #[test]
    fn median_passes_a_sustained_change() {
        let mut median = Median3::default();
        median.update(1.0);
        median.update(1.0);
        median.update(5.0);
        assert_eq!(median.update(5.0), 5.0);
    }

    #[test]
    fn ema_first_sample_is_taken_as_is() {
        let mut ema = Ema::default();
        assert_eq!(ema.update(10.0, 0, 60.0), 10.0);
    }

    #[test]
    fn ema_reaches_63_percent_after_one_time_constant() {
        let mut ema = Ema::default();
        ema.update(0.0, 0, 60.0);
        for i in 1..=12 {
            ema.update(1.0, i * 5000, 60.0);
        }
        let value = ema.value().unwrap();
        assert!((value - 0.632).abs() < 0.01, "{value}");
    }

    #[test]
    fn ema_rate_is_independent_of_sample_interval() {
        let mut fast = Ema::default();
        let mut slow = Ema::default();
        fast.update(0.0, 0, 30.0);
        slow.update(0.0, 0, 30.0);
        for i in 1..=60 {
            fast.update(1.0, i * 1000, 30.0);
        }
        for i in 1..=6 {
            slow.update(1.0, i * 10_000, 30.0);
        }
        assert!((fast.value().unwrap() - slow.value().unwrap()).abs() < 1e-4);
    }

    #[test]
    fn zero_smoothing_follows_samples() {
        let mut ema = Ema::default();
        ema.update(0.0, 0, 0.0);
        assert_eq!(ema.update(5.0, 5000, 0.0), 5.0);
    }

    #[test]
    fn dull_day_stays_open() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        assert_eq!(settle(&mut controller, &config, sample(30.0, 200.0), 50), 1.0);
    }

    #[test]
    fn hot_and_sunny_closes_fully() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        assert_eq!(settle(&mut controller, &config, sample(28.0, 5000.0), 100), 0.0);
    }

    #[test]
    fn cold_and_sunny_opens_fully() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        assert_eq!(settle(&mut controller, &config, sample(16.0, 5000.0), 100), 1.0);
    }

    #[test]
    fn comfortable_and_sunny_cuts_glare() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        let setpoint = settle(&mut controller, &config, sample(22.0, 5000.0), 100);
        assert!((setpoint - (1.0 - config.glare_shade)).abs() < 1e-4, "{setpoint}");
    }

    #[test]
    fn warmer_closes_more() {
        let config = BlindConfig::default();
        let mut mild = Controller::default();
        let mut warm = Controller::default();
        let mild = settle(&mut mild, &config, sample(24.5, 1000.0), 100);
        let warm = settle(&mut warm, &config, sample(25.5, 1000.0), 100);
        assert!(warm < mild, "{warm} {mild}");
    }

    #[test]
    fn night_uses_night_position_with_hysteresis() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        settle(&mut controller, &config, sample(22.0, 5.0), 50);
        assert_eq!(controller.state().night, 1);

        // Above night_lux but inside the hysteresis band: still night.
        settle(&mut controller, &config, sample(22.0, 30.0), 50);
        assert_eq!(controller.state().night, 1);

        settle(&mut controller, &config, sample(22.0, 100.0), 50);
        assert_eq!(controller.state().night, 0);
    }

    #[test]
    fn deadband_ignores_small_changes() {
        let config = BlindConfig {
            temp_smoothing_s: 0.0,
            light_smoothing_s: 0.0,
            ..BlindConfig::default()
        };
        let mut controller = Controller::default();
        let first = settle(&mut controller, &config, sample(22.0, 1000.0), 1);
        let second = settle(&mut controller, &config, sample(22.0, 1050.0), 1);
        assert_eq!(first, second);
        assert_ne!(controller.state().target, first);
    }

    #[test]
    fn smoothing_rejects_a_brief_spike() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        let before = settle(&mut controller, &config, sample(22.0, 400.0), 50);
        let during = settle(&mut controller, &config, sample(22.0, 20_000.0), 1);
        assert_eq!(before, during);
    }

    #[test]
    fn implausible_temperature_is_ignored() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        settle(&mut controller, &config, sample(22.0, 1000.0), 20);
        settle(&mut controller, &config, sample(98.0, 1000.0), 20);
        assert_eq!(controller.state().sensor_fault, 1);
        assert!((controller.state().temperature_avg - 22.0).abs() < 1e-3);
    }

    #[test]
    fn no_setpoint_until_a_plausible_sample() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        assert_eq!(controller.step(&config, sample(f32::NAN, 100.0), 0), None);
    }

    #[test]
    fn manual_holds_until_auto() {
        let config = BlindConfig::default();
        let mut controller = Controller::default();
        settle(&mut controller, &config, sample(28.0, 5000.0), 50);
        controller.set_manual(Some(0.8), 250_000);
        assert_eq!(settle(&mut controller, &config, sample(28.0, 5000.0), 50), 0.8);
        assert_eq!(controller.set_auto(), Some(0.0));
    }

    #[test]
    fn manual_times_out() {
        let config = BlindConfig {
            manual_timeout_s: 60.0,
            ..BlindConfig::default()
        };
        let mut controller = Controller::default();
        settle(&mut controller, &config, sample(28.0, 5000.0), 50);
        controller.set_manual(Some(0.8), 250_000);
        settle(&mut controller, &config, sample(28.0, 5000.0), 11);
        assert_eq!(controller.mode(), Mode::Manual);
        assert_eq!(settle(&mut controller, &config, sample(28.0, 5000.0), 2), 0.0);
        assert_eq!(controller.mode(), Mode::Auto);
    }

    #[test]
    fn update_merges_and_validates() {
        let config = BlindConfig::default();
        let update = BlindConfigUpdate {
            temp_high: Some(26.0),
            ..BlindConfigUpdate::default()
        };
        assert_eq!(config.updated(&update).unwrap().temp_high, 26.0);

        let bad = BlindConfigUpdate {
            temp_low: Some(30.0),
            ..BlindConfigUpdate::default()
        };
        assert!(config.updated(&bad).is_err());
    }

    #[test]
    fn reset_restores_defaults() {
        let config = BlindConfig {
            temp_high: 30.0,
            ..BlindConfig::default()
        };
        let update = BlindConfigUpdate {
            reset: true,
            ..BlindConfigUpdate::default()
        };
        assert_eq!(config.updated(&update).unwrap(), BlindConfig::default());
    }
}
