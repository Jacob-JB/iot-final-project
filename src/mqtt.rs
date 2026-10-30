use alloc::{format, string::String};
use core::{convert::Infallible, fmt::Write};

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::{Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal, watch::Watch};
use embassy_time::{Duration, Timer};
use rust_mqtt::{
    Bytes,
    buffer::AllocBuffer,
    client::{
        Client,
        event::Event,
        options::{ConnectOptions, PublicationOptions, SubscriptionOptions, TopicReference},
    },
    types::{MqttString, TopicName},
};
use sequential_storage::map::{SerializationError, Value};
use serde::Deserialize;

use crate::{
    sensors::SensorReading,
    servo,
    storage::{self, StorageError, StorageKey, deserialize_str, serialize_str},
};

const DEFAULT_BROKER_HOST: &str = "broker.emqx.io";
const DEFAULT_BROKER_PORT: u16 = 1883;
const DEFAULT_TOPIC_PREFIX: &str = "jacob-brendan/smartblind";

/// Joined to [`MqttConfig::device_id`] with a `-` to form the client ID.
const CLIENT_ID_PREFIX: &str = "jacob-brendan-smartblind";

/// Topic level in place of a device ID that addresses every device under the same
/// [`MqttConfig::topic_prefix`].
const BROADCAST_ID: &str = "all";

/// Device topic that each [`SensorReading`] is published to as JSON.
const SENSOR_SUBTOPIC: &str = "sensors";

/// Device topic that [`Command`]s are received on, both under this device's ID and under
/// [`BROADCAST_ID`].
const COMMAND_SUBTOPIC: &str = "command";

/// Delay before reconnecting after the broker connection fails or drops.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Time without progress on the broker socket after which the connection counts as dropped.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// The config the client connects with. Empty until [`mqtt_task`] loads it from flash.
static MQTT_CONFIG: Watch<CriticalSectionRawMutex, MqttConfig, 1> = Watch::new();

/// The latest reading not yet published.
static PENDING_READING: Signal<CriticalSectionRawMutex, SensorReading> = Signal::new();

/// A command received over MQTT as JSON.
#[derive(Deserialize)]
struct Command {
    /// Servo position from 0 to 1, as described by [`servo::Servo::write_position`].
    position: f32,
}

#[derive(Clone)]
pub struct MqttConfig {
    pub broker_host: heapless::String<64>,
    pub broker_port: u16,
    /// Device topics are named `<topic_prefix>/<device_id>/<subtopic>`. It must be a valid topic
    /// name.
    pub topic_prefix: heapless::String<64>,
    /// Must satisfy [`is_valid_device_id`]. It must also be unique among the devices sharing a
    /// broker, because it is part of the client ID.
    pub device_id: heapless::String<32>,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            broker_host: heapless::String::try_from(DEFAULT_BROKER_HOST).unwrap(),
            broker_port: DEFAULT_BROKER_PORT,
            topic_prefix: heapless::String::try_from(DEFAULT_TOPIC_PREFIX).unwrap(),
            device_id: default_device_id(),
        }
    }
}

/// Returns the last three bytes of the chip's base MAC address in lowercase hex.
fn default_device_id() -> heapless::String<32> {
    let mac = esp_hal::efuse::base_mac_address();
    let mut id = heapless::String::new();

    for byte in &mac.as_bytes()[3..] {
        write!(id, "{:02x}", byte).unwrap();
    }

    id
}

/// Returns whether `id` is a single non-empty topic level other than the broadcast ID `all`.
pub fn is_valid_device_id(id: &str) -> bool {
    !id.is_empty() && !id.contains(['/', '+', '#']) && id != BROADCAST_ID
}

impl<'a> Value<'a> for MqttConfig {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, SerializationError> {
        let host_len = serialize_str(&self.broker_host, buffer)?;
        let port = buffer
            .get_mut(host_len..host_len + 2)
            .ok_or(SerializationError::BufferTooSmall)?;
        port.copy_from_slice(&self.broker_port.to_le_bytes());
        let prefix_len = serialize_str(&self.topic_prefix, &mut buffer[host_len + 2..])?;
        let id_len = serialize_str(&self.device_id, &mut buffer[host_len + 2 + prefix_len..])?;

        Ok(host_len + 2 + prefix_len + id_len)
    }

    fn deserialize_from(buffer: &'a [u8]) -> Result<(Self, usize), SerializationError> {
        let (broker_host, host_len) = deserialize_str(buffer)?;
        let port = buffer
            .get(host_len..host_len + 2)
            .ok_or(SerializationError::InvalidFormat)?;
        let broker_port = u16::from_le_bytes([port[0], port[1]]);
        let (topic_prefix, prefix_len) = deserialize_str(&buffer[host_len + 2..])?;
        let (device_id, id_len) = deserialize_str(&buffer[host_len + 2 + prefix_len..])?;

        Ok((
            Self {
                broker_host,
                broker_port,
                topic_prefix,
                device_id,
            },
            host_len + 2 + prefix_len + id_len,
        ))
    }
}

/// Saves `config` to flash, then reconnects to the broker with it.
///
/// The client keeps its current config if saving fails.
pub async fn set_mqtt_config(config: MqttConfig) -> Result<(), StorageError> {
    storage::store(StorageKey::MqttConfig, &config).await?;
    MQTT_CONFIG.sender().send(config);
    Ok(())
}

/// Returns `None` until the config has been loaded from flash.
pub fn mqtt_config() -> Option<MqttConfig> {
    MQTT_CONFIG.try_get()
}

/// Queues `reading` to be published, replacing any reading still queued.
///
/// Only the newest reading is published once the broker connection comes back after a drop.
pub fn publish_reading(reading: SensorReading) {
    PENDING_READING.signal(reading);
}

/// Starts publishing the readings given to [`publish_reading`] and applying received
/// [`Command`]s whenever `stack` is connected.
///
/// Requires [`storage::init`] to have been called.
///
/// # Panics
///
/// Panics if called more than once.
pub fn init(spawner: Spawner, stack: Stack<'static>) {
    spawner.spawn(mqtt_task(stack).expect("failed to spawn the MQTT task"));
}

/// Keeps a broker connection open using the latest [`MQTT_CONFIG`], reconnecting whenever the
/// connection drops or the config changes.
#[embassy_executor::task]
async fn mqtt_task(stack: Stack<'static>) -> ! {
    let mut receiver = MQTT_CONFIG
        .receiver()
        .expect("MQTT config receiver already taken");

    let stored = storage::load::<MqttConfig>(StorageKey::MqttConfig).await;
    MQTT_CONFIG.sender().send(stored.unwrap_or_default());

    let mut config = receiver.get().await;

    loop {
        match select(run_session(stack, &config), receiver.changed()).await {
            Either::First(Ok(never)) => match never {},
            Either::First(Err(e)) => {
                warn!("MQTT connection ended: {}", e);

                if let Either::Second(new_config) =
                    select(Timer::after(RECONNECT_DELAY), receiver.changed()).await
                {
                    config = new_config;
                }
            }
            Either::Second(new_config) => config = new_config,
        }
    }
}

/// Waits for the network, connects to the broker, then publishes readings and applies commands
/// until the connection fails.
#[allow(
    clippy::large_stack_frames,
    reason = "the socket buffers live on the task's stack"
)]
async fn run_session(
    stack: Stack<'static>,
    config: &MqttConfig,
) -> Result<Infallible, &'static str> {
    stack.wait_config_up().await;

    let broker_address = *stack
        .dns_query(&config.broker_host, DnsQueryType::A)
        .await
        .map_err(|_| "failed to resolve the broker's hostname")?
        .first()
        .ok_or("DNS query for the broker returned no addresses")?;

    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(SOCKET_TIMEOUT));

    info!(
        "connecting to MQTT broker at {} ({}:{})",
        config.broker_host, broker_address, config.broker_port
    );

    socket
        .connect((broker_address, config.broker_port))
        .await
        .map_err(|_| "failed to open a TCP connection to the broker")?;

    let mut buffer = AllocBuffer;
    let mut client = Client::<'_, _, _, 2, 1, 1, 1>::new(&mut buffer);

    let client_id = format!("{}-{}", CLIENT_ID_PREFIX, config.device_id);
    client
        .connect(
            socket,
            &ConnectOptions::new().clean_start(),
            Some(
                MqttString::try_from(client_id.as_str())
                    .map_err(|_| "the client ID is too long")?,
            ),
        )
        .await
        .map_err(|_| "failed to connect to the broker")?;

    info!("connected to MQTT broker as {}", client_id.as_str());

    let topic = |device_id: &str, subtopic: &str| -> String {
        format!("{}/{}/{}", config.topic_prefix, device_id, subtopic)
    };

    for command_topic in [
        topic(&config.device_id, COMMAND_SUBTOPIC),
        topic(BROADCAST_ID, COMMAND_SUBTOPIC),
    ] {
        let topic_name =
            topic_name(&command_topic).ok_or("a command topic is not a valid topic name")?;

        client
            .subscribe(topic_name.into(), SubscriptionOptions::new())
            .await
            .map_err(|_| "failed to subscribe to a command topic")?;

        info!("subscribing to {}", command_topic.as_str());
    }

    let sensor_topic = topic(&config.device_id, SENSOR_SUBTOPIC);
    let sensor_topic =
        topic_name(&sensor_topic).ok_or("the sensor topic is not a valid topic name")?;
    let publish_options = PublicationOptions::new(TopicReference::Name(sensor_topic)).retain();

    loop {
        match select(client.poll(), PENDING_READING.wait()).await {
            Either::First(event) => match event.map_err(|_| "failed to poll the broker")? {
                Event::Publish(publish) => apply_command(&publish.message),
                Event::Suback(suback) if suback.reason_code.is_erroneous() => {
                    return Err("the broker rejected a command subscription");
                }
                _ => {}
            },
            Either::Second(reading) => {
                let payload = serde_json::to_string(&reading)
                    .map_err(|_| "failed to encode the sensor reading")?;

                client
                    .publish(&publish_options, Bytes::from(payload.as_str()))
                    .await
                    .map_err(|_| "failed to publish the sensor reading")?;
            }
        }
    }
}

/// Returns `None` if `topic` is not a valid topic name.
fn topic_name(topic: &str) -> Option<TopicName<'_>> {
    MqttString::try_from(topic).ok().and_then(TopicName::new)
}

/// Applies `payload` if it is a valid [`Command`] JSON object.
fn apply_command(payload: &[u8]) {
    match serde_json::from_slice::<Command>(payload) {
        Ok(Command { position }) if (0.0..=1.0).contains(&position) => {
            info!("servo position set over MQTT to {}", position);
            servo::set_position(position);
        }
        Ok(Command { position }) => {
            warn!(
                "rejected servo position {} because it is outside 0 to 1",
                position
            )
        }
        Err(_) => warn!("received a command that is not a valid command object"),
    }
}
