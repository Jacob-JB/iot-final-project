use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
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

/// The latest position given to [`set_position`] that the servo task hasn't picked up.
static TARGET_POSITION: Signal<CriticalSectionRawMutex, f32> = Signal::new();

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

    /// Sends frames with a high pulse of `pulse_us` microseconds.
    ///
    /// `pulse_us` is clamped to the range between [`MIN_PULSE_US`] and [`MAX_PULSE_US`].
    pub async fn write_pulse(&mut self, pulse_us: u16) {
        let pulse_us = pulse_us.clamp(MIN_PULSE_US, MAX_PULSE_US);
        let frame = [
            PulseCode::new(
                gpio::Level::High,
                pulse_us,
                gpio::Level::Low,
                FRAME_US - pulse_us,
            ),
            PulseCode::end_marker(),
        ];

        let _ = self.rmt_channel.transmit(&frame).await;
    }

    /// Sends frames for `position`, where 0 is the [`MIN_PULSE_US`] end and 1 is the
    /// [`MAX_PULSE_US`] end.
    ///
    /// `position` is clamped to the range between 0 and 1.
    pub async fn write_position(&mut self, position: f32) {
        let span = f32::from(MAX_PULSE_US - MIN_PULSE_US);
        let pulse_us = MIN_PULSE_US + (position.clamp(0.0, 1.0) * span) as u16;

        self.write_pulse(pulse_us).await;
    }
}

/// Moves the servo to `position` and holds it there, as described by [`Servo::write_position`].
pub fn set_position(position: f32) {
    TARGET_POSITION.signal(position);
}

/// Starts driving the servo connected to `pin` to the positions given to [`set_position`].
///
/// The servo is not driven until the first position is set.
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
    let mut position = TARGET_POSITION.wait().await;

    loop {
        if let Some(new_position) = TARGET_POSITION.try_take() {
            position = new_position;
        }

        servo.write_position(position).await;
    }
}
