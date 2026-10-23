use embassy_executor::Spawner;
use embassy_time::{Duration, Instant};
use esp_hal::{
    Async,
    gpio::{self, interconnect::PeripheralOutput},
    rmt::{self, PulseCode, TxChannelCreator},
};

/// Length of one servo frame in RMT ticks. One tick is 1 µs.
const FRAME_US: u16 = 20_000;

/// Pulse width for the servo's minimum position.
const MIN_PULSE_US: u16 = 1_000;

/// Pulse width for the servo's maximum position.
const MAX_PULSE_US: u16 = 2_000;

/// How long the servo holds each end of its sweep.
const HOLD_TIME: Duration = Duration::from_secs(2);

/// Drives an SG90 servo with a 50 Hz pulse train.
pub struct Servo<'d> {
    rmt_channel: rmt::Channel<'d, Async, rmt::Tx>,
}

impl<'d> Servo<'d> {
    /// Creates a servo on `pin`. The RMT peripheral must be clocked at 80 MHz.
    pub fn new(
        rmt_channel: impl TxChannelCreator<'d, Async>,
        pin: impl PeripheralOutput<'d>,
    ) -> Self {
        let channel = rmt_channel
            .configure_tx(&rmt::TxChannelConfig::default().with_clk_divider(80))
            .expect("failed to configure the servo RMT channel")
            .with_pin(pin);

        Servo {
            rmt_channel: channel,
        }
    }

    /// Sends frames with a high pulse of `pulse_us` microseconds until `duration` has elapsed.
    ///
    /// `pulse_us` is clamped to the range between [`MIN_PULSE_US`] and [`MAX_PULSE_US`].
    pub async fn hold(&mut self, pulse_us: u16, duration: Duration) {
        let pulse_us = pulse_us.clamp(MIN_PULSE_US, MAX_PULSE_US);
        let frame = [
            PulseCode::new(gpio::Level::High, pulse_us, gpio::Level::Low, FRAME_US - pulse_us),
            PulseCode::end_marker(),
        ];

        let end = Instant::now() + duration;
        while Instant::now() < end {
            let _ = self.rmt_channel.transmit(&frame).await;
        }
    }
}

/// Starts sweeping the servo connected to `pin` between its end positions.
///
/// The RMT peripheral must be clocked at 80 MHz.
///
/// # Panics
///
/// Panics if called more than once.
pub fn init(
    spawner: Spawner,
    rmt_channel: impl TxChannelCreator<'static, Async>,
    pin: impl PeripheralOutput<'static>,
) {
    let servo = Servo::new(rmt_channel, pin);

    spawner.spawn(servo_task(servo).expect("failed to spawn the servo task"));
}

#[embassy_executor::task]
async fn servo_task(mut servo: Servo<'static>) -> ! {
    loop {
        servo.hold(MIN_PULSE_US, HOLD_TIME).await;
        servo.hold(MAX_PULSE_US, HOLD_TIME).await;
    }
}
