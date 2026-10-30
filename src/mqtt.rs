use alloc::format;
use core::convert::Infallible;

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
        options::{ConnectOptions, PublicationOptions, TopicReference},
    },
    types::{MqttString, TopicName},
};
use sequential_storage::map::{SerializationError, Value};

use crate::{
    sensors::SensorReading,
    storage::{self, StorageError, StorageKey, deserialize_str, serialize_str},
};

const DEFAULT_BROKER_HOST: &str = "broker.emqx.io";
const DEFAULT_BROKER_PORT: u16 = 1883;
const DEFAULT_TOPIC_PREFIX: &str = "jacob-brendan/smartblind";

/// Must be unique among the clients connected to the broker.
const CLIENT_ID: &str = "jacob-brendan-smartblind";

/// Topic under [`MqttConfig::topic_prefix`] that each [`SensorReading`] is published to as JSON.
const SENSOR_SUBTOPIC: &str = "sensors";

/// Delay before reconnecting after the broker connection fails or drops.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Time without progress on the broker socket after which the connection counts as dropped.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

/// The config the client connects with. Empty until [`mqtt_task`] loads it from flash.
static MQTT_CONFIG: Watch<CriticalSectionRawMutex, MqttConfig, 1> = Watch::new();

/// The latest reading not yet published.
static PENDING_READING: Signal<CriticalSectionRawMutex, SensorReading> = Signal::new();

#[derive(Clone)]
pub struct MqttConfig {
    pub broker_host: heapless::String<64>,
    pub broker_port: u16,
    /// Topics are published under this prefix followed by a `/`. It must be a valid topic name.
    pub topic_prefix: heapless::String<64>,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            broker_host: heapless::String::try_from(DEFAULT_BROKER_HOST).unwrap(),
            broker_port: DEFAULT_BROKER_PORT,
            topic_prefix: heapless::String::try_from(DEFAULT_TOPIC_PREFIX).unwrap(),
        }
    }
}

impl<'a> Value<'a> for MqttConfig {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, SerializationError> {
        let host_len = serialize_str(&self.broker_host, buffer)?;
        let port = buffer
            .get_mut(host_len..host_len + 2)
            .ok_or(SerializationError::BufferTooSmall)?;
        port.copy_from_slice(&self.broker_port.to_le_bytes());
        let prefix_len = serialize_str(&self.topic_prefix, &mut buffer[host_len + 2..])?;

        Ok(host_len + 2 + prefix_len)
    }

    fn deserialize_from(buffer: &'a [u8]) -> Result<(Self, usize), SerializationError> {
        let (broker_host, host_len) = deserialize_str(buffer)?;
        let port = buffer
            .get(host_len..host_len + 2)
            .ok_or(SerializationError::InvalidFormat)?;
        let broker_port = u16::from_le_bytes([port[0], port[1]]);
        let (topic_prefix, prefix_len) = deserialize_str(&buffer[host_len + 2..])?;

        Ok((
            Self {
                broker_host,
                broker_port,
                topic_prefix,
            },
            host_len + 2 + prefix_len,
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

/// Starts publishing the readings given to [`publish_reading`] whenever `stack` is connected.
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

/// Waits for the network, connects to the broker and publishes readings until the connection
/// fails.
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
    let mut client = Client::<'_, _, _, 1, 1, 1, 1>::new(&mut buffer);

    client
        .connect(
            socket,
            &ConnectOptions::new().clean_start(),
            Some(MqttString::try_from(CLIENT_ID).unwrap()),
        )
        .await
        .map_err(|_| "failed to connect to the broker")?;

    info!("connected to MQTT broker");

    let topic = format!("{}/{}", config.topic_prefix, SENSOR_SUBTOPIC);
    let topic = MqttString::try_from(topic.as_str())
        .ok()
        .and_then(TopicName::new)
        .ok_or("the sensor topic is not a valid topic name")?;
    let publish_options = PublicationOptions::new(TopicReference::Name(topic)).retain();

    loop {
        match select(client.poll(), PENDING_READING.wait()).await {
            Either::First(event) => {
                event.map_err(|_| "failed to poll the broker")?;
            }
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
