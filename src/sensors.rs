use defmt::info;
use embassy_executor::Spawner;
use embassy_time::{Duration, Ticker};
use serde::Serialize;
use esp_hal::{
    Async,
    analog::adc::{Adc, AdcCalCurve, AdcChannel, AdcConfig, AdcPin, Attenuation},
    peripherals::{ADC1, GPIO4, GPIO5},
};

use crate::mqtt;

/// Time between sensor readings.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Voltage powering the sensor voltage dividers, in millivolts.
const SUPPLY_MV: f32 = 3300.0;

/// Resistance of the fixed resistor in each sensor voltage divider, in ohms.
const DIVIDER_OHMS: f32 = 10_000.0;

/// Thermistor resistance at [`THERMISTOR_NOMINAL_KELVIN`], in ohms.
const THERMISTOR_NOMINAL_OHMS: f32 = 10_000.0;

/// Temperature at which the thermistor has [`THERMISTOR_NOMINAL_OHMS`] of resistance.
const THERMISTOR_NOMINAL_KELVIN: f32 = 298.15;

/// B coefficient of the thermistor, in kelvin.
const THERMISTOR_BETA: f32 = 3950.0;

/// Photoresistor resistance at 10 lux, in ohms.
const PHOTORESISTOR_OHMS_AT_10_LUX: f32 = 15_000.0;

/// Slope of the photoresistor's log resistance against log illuminance.
const PHOTORESISTOR_GAMMA: f32 = 0.6;

/// One reading of every sensor.
#[derive(Clone, Copy, Serialize)]
pub struct SensorReading {
    /// Room temperature in degrees Celsius.
    pub temperature: f32,
    /// Approximate window brightness in lux.
    pub light: f32,
}

/// A pin enabled on ADC1 whose readings are calibrated to millivolts.
pub type CalibratedPin<PIN> = AdcPin<PIN, ADC1<'static>, AdcCalCurve<ADC1<'static>>>;

/// A resistive sensor in a voltage divider on ADC1.
///
/// The sensor must sit between the supply and the ADC pin, with a [`DIVIDER_OHMS`] resistor
/// between the pin and ground.
pub struct AnalogSensor<PIN> {
    pin: CalibratedPin<PIN>,
}

impl<PIN: AdcChannel> AnalogSensor<PIN> {
    pub fn new(pin: CalibratedPin<PIN>) -> Self {
        AnalogSensor { pin }
    }

    /// Returns the sensor resistance in ohms, or infinity if the divider output reads zero.
    pub async fn read_resistance(&mut self, adc: &mut Adc<'_, ADC1<'static>, Async>) -> f32 {
        let millivolts = f32::from(adc.read_oneshot(&mut self.pin).await);

        DIVIDER_OHMS * (SUPPLY_MV - millivolts) / millivolts
    }

    /// Returns the temperature in degrees Celsius, treating the sensor as a 10 kΩ NTC thermistor.
    pub async fn read_temperature(&mut self, adc: &mut Adc<'_, ADC1<'static>, Async>) -> f32 {
        let ohms = self.read_resistance(adc).await;
        let kelvin = 1.0
            / (1.0 / THERMISTOR_NOMINAL_KELVIN
                + libm::logf(ohms / THERMISTOR_NOMINAL_OHMS) / THERMISTOR_BETA);

        kelvin - 273.15
    }

    /// Returns the approximate illuminance in lux, treating the sensor as a GL5528 photoresistor.
    ///
    /// Accuracy is limited by the part-to-part spread of the photoresistor, so the result is
    /// suited to comparing brightness rather than measuring it.
    pub async fn read_light(&mut self, adc: &mut Adc<'_, ADC1<'static>, Async>) -> f32 {
        let ohms = self.read_resistance(adc).await;

        10.0 * libm::powf(PHOTORESISTOR_OHMS_AT_10_LUX / ohms, 1.0 / PHOTORESISTOR_GAMMA)
    }
}

/// Starts reading the thermistor on GPIO4 and the photoresistor on GPIO5 every
/// [`POLL_INTERVAL`], and publishing each reading with [`mqtt::publish_reading`].
///
/// # Panics
///
/// Panics if called more than once.
pub fn init(
    spawner: Spawner,
    adc: ADC1<'static>,
    thermistor_pin: GPIO4<'static>,
    photoresistor_pin: GPIO5<'static>,
) {
    let mut config = AdcConfig::new();
    let thermistor = config.enable_pin_with_cal(thermistor_pin, Attenuation::_11dB);
    let photoresistor = config.enable_pin_with_cal(photoresistor_pin, Attenuation::_11dB);
    let adc = Adc::new(adc, config).into_async();

    let thermistor = AnalogSensor::new(thermistor);
    let photoresistor = AnalogSensor::new(photoresistor);

    spawner.spawn(
        sensor_task(adc, thermistor, photoresistor).expect("failed to spawn the sensor task"),
    );
}

#[embassy_executor::task]
async fn sensor_task(
    mut adc: Adc<'static, ADC1<'static>, Async>,
    mut thermistor: AnalogSensor<GPIO4<'static>>,
    mut photoresistor: AnalogSensor<GPIO5<'static>>,
) -> ! {
    let mut ticker = Ticker::every(POLL_INTERVAL);

    loop {
        let temperature = thermistor.read_temperature(&mut adc).await;
        let light = photoresistor.read_light(&mut adc).await;
        info!("temperature: {} °C, light: {} lux", temperature, light);
        mqtt::publish_reading(SensorReading { temperature, light });

        ticker.next().await;
    }
}
