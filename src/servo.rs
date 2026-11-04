use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::Duration;
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

/// The latest target given to [`set_position`] that the servo task hasn't picked up.
static TARGET: Signal<CriticalSectionRawMutex, Target> = Signal::new();

/// Bits of the `f32` position the servo is currently driven to, or of NaN before it is driven.
static POSITION: AtomicU32 = AtomicU32::new(f32::NAN.to_bits());

#[derive(Clone, Copy)]
struct Target {
    position: f32,
    travel_time: Duration,
}

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
///
/// The servo moves at a constant speed that would take `travel_time` to cover the full range,
/// or jumps straight there if `travel_time` is zero.
pub fn set_position(position: f32, travel_time: Duration) {
    TARGET.signal(Target {
        position,
        travel_time,
    });
}

/// Returns the position the servo is currently driven to, or `None` before the first
/// [`set_position`].
pub fn position() -> Option<f32> {
    let position = f32::from_bits(POSITION.load(Ordering::Relaxed));
    (!position.is_nan()).then_some(position)
}

/// Starts driving the servo connected to `pin` to the positions given to [`set_position`].
///
/// The servo is not driven until the first position is set, which it jumps straight to.
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
    let mut target = TARGET.wait().await;
    let mut position = target.position;

    loop {
        if let Some(new_target) = TARGET.try_take() {
            target = new_target;
        }

        // Each frame takes FRAME_US to send, so this is how far one frame may move.
        let max_step = match target.travel_time.as_micros() {
            0 => f32::INFINITY,
            travel_us => f32::from(FRAME_US) / travel_us as f32,
        };
        position += (target.position - position).clamp(-max_step, max_step);
        POSITION.store(position.to_bits(), Ordering::Relaxed);

        servo.write_position(position).await;
    }
}
