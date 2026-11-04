use defmt::info;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal};
use embassy_time::{Duration, Instant};
use serde::Deserialize;

use crate::{
    controller::{BlindConfig, Controller, Mode, Sample},
    sensors::SensorReading,
    servo,
};

/// The latest reading not yet processed.
static PENDING_READING: Signal<CriticalSectionRawMutex, SensorReading> = Signal::new();

/// Commands waiting for [`blind_task`].
static COMMANDS: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();

/// A command received as JSON over MQTT.
///
/// `{"position": 0.4}` holds the blind at 40 % open in manual mode, `{"mode": "manual"}` holds
/// it where it is, and `{"mode": "auto"}` hands control back to the sensors.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    /// Openness from 0 (closed) to 1 (open).
    position: Option<f32>,
    mode: Option<Mode>,
}

/// Queues `reading` for the controller, replacing any reading still queued.
pub fn submit_reading(reading: SensorReading) {
    PENDING_READING.signal(reading);
}

/// Parses and queues a [`Command`] JSON object.
pub fn handle_command(json: &[u8]) -> Result<(), &'static str> {
    let command = serde_json::from_slice::<Command>(json).map_err(|_| "not a valid command")?;

    match command {
        Command {
            position: Some(position),
            mode: None | Some(Mode::Manual),
        } if !(0.0..=1.0).contains(&position) => Err("position must be between 0 and 1"),
        Command {
            position: Some(_),
            mode: Some(Mode::Auto),
        } => Err("a position can't be set in auto mode"),
        Command {
            position: None,
            mode: None,
        } => Err("a command needs a position or a mode"),
        command => COMMANDS
            .try_send(command)
            .map_err(|_| "too many commands in flight, try again"),
    }
}

/// Starts controlling the blind from the readings given to [`submit_reading`] and the commands
/// given to [`handle_command`], using the [`BlindConfig`] defaults.
///
/// # Panics
///
/// Panics if called more than once.
pub fn init(spawner: Spawner) {
    spawner.spawn(blind_task().expect("failed to spawn the blind task"));
}

#[embassy_executor::task]
async fn blind_task() -> ! {
    let config = BlindConfig::default();
    let mut controller = Controller::default();

    loop {
        let setpoint = match select(PENDING_READING.wait(), COMMANDS.receive()).await {
            Either::First(reading) => {
                let sample = Sample {
                    temperature: reading.temperature,
                    light: reading.light,
                };
                let setpoint = controller.step(&config, sample, Instant::now().as_millis());
                log_state(&config, &controller, reading);
                setpoint
            }
            Either::Second(command) => match command.mode {
                Some(Mode::Auto) => {
                    info!("blind set to auto");
                    controller.set_auto()
                }
                _ => {
                    info!("blind set to manual at {}", command.position);
                    controller.set_manual(command.position, Instant::now().as_millis())
                }
            },
        };

        if let Some(setpoint) = setpoint {
            drive(&config, setpoint);
        }
    }
}

/// Moves the servo to `openness`, accounting for [`BlindConfig::invert`].
fn drive(config: &BlindConfig, openness: f32) {
    let travel_time = Duration::from_millis((config.travel_s * 1000.0) as u64);
    servo::set_position(to_servo(config, openness), travel_time);
}

/// Converts between openness and servo position, which are the same map in both directions.
fn to_servo(config: &BlindConfig, position: f32) -> f32 {
    if config.invert {
        1.0 - position
    } else {
        position
    }
}

fn log_state(config: &BlindConfig, controller: &Controller, reading: SensorReading) {
    let state = controller.state();
    let position = servo::position()
        .map(|position| to_servo(config, position))
        .unwrap_or(f32::NAN);

    info!(
        "{} °C (avg {}), {} lux (avg {}) -> target {}, setpoint {}, position {}, {}",
        reading.temperature,
        state.temperature_avg,
        reading.light,
        state.light_avg,
        state.target,
        state.setpoint,
        position,
        match controller.mode() {
            Mode::Auto => "auto",
            Mode::Manual => "manual",
        }
    );
}
