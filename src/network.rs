use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::StackResources;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{
    AuthenticationMethod, Config, WifiController, WifiError, sta::StationConfig,
};
use sequential_storage::map::{SerializationError, Value};
use static_cell::StaticCell;

use crate::storage::{self, StorageError, StorageKey};

static NETWORK_STACK_RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();

/// The credentials the station connects with. Empty until credentials are loaded or set.
static WIFI_CREDENTIALS: Watch<CriticalSectionRawMutex, WifiCredentials, 1> = Watch::new();

/// Delay before retrying after a failed connection attempt or a dropped connection.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct WifiCredentials {
    pub ssid: heapless::String<32>,
    /// Empty for an open network.
    pub password: heapless::String<64>,
}

impl<'a> Value<'a> for WifiCredentials {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, SerializationError> {
        let ssid_len = serialize_str(&self.ssid, buffer)?;
        let password_len = serialize_str(
            &self.password,
            buffer
                .get_mut(ssid_len..)
                .ok_or(SerializationError::BufferTooSmall)?,
        )?;

        Ok(ssid_len + password_len)
    }

    fn deserialize_from(buffer: &'a [u8]) -> Result<(Self, usize), SerializationError> {
        let (ssid, ssid_len) = deserialize_str(buffer)?;
        let (password, password_len) = deserialize_str(&buffer[ssid_len..])?;

        Ok((Self { ssid, password }, ssid_len + password_len))
    }
}

/// Writes `s` as a one byte length followed by its bytes. `s` must be at most 255 bytes.
fn serialize_str(s: &str, buffer: &mut [u8]) -> Result<usize, SerializationError> {
    let len = u8::try_from(s.len()).map_err(|_| SerializationError::InvalidData)?;
    let out = buffer
        .get_mut(..1 + s.len())
        .ok_or(SerializationError::BufferTooSmall)?;

    out[0] = len;
    out[1..].copy_from_slice(s.as_bytes());

    Ok(out.len())
}

fn deserialize_str<const N: usize>(
    buffer: &[u8],
) -> Result<(heapless::String<N>, usize), SerializationError> {
    let (&len, rest) = buffer
        .split_first()
        .ok_or(SerializationError::InvalidFormat)?;
    let bytes = rest
        .get(..len as usize)
        .ok_or(SerializationError::InvalidFormat)?;
    let s = core::str::from_utf8(bytes).map_err(|_| SerializationError::InvalidFormat)?;
    let s = heapless::String::try_from(s).map_err(|_| SerializationError::InvalidFormat)?;

    Ok((s, 1 + bytes.len()))
}

/// Saves `credentials` to flash, then reconnects the station with them.
///
/// The station keeps its current credentials if saving fails.
pub async fn set_wifi_credentials(credentials: WifiCredentials) -> Result<(), StorageError> {
    storage::store(StorageKey::WifiCredentials, &credentials).await?;
    WIFI_CREDENTIALS.sender().send(credentials);
    Ok(())
}

pub fn wifi_credentials() -> Option<WifiCredentials> {
    WIFI_CREDENTIALS.try_get()
}

/// Requires [`storage::init`] to have been called.
pub fn init_network(spawner: Spawner, wifi_peripheral: esp_hal::peripherals::WIFI<'static>) {
    let rng = esp_hal::rng::Rng::new();

    let (wifi_controller, interfaces) = esp_radio::wifi::new(wifi_peripheral, Default::default())
        .expect("Failed to initialize Wi-Fi controller");

    let net_seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (_stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        NETWORK_STACK_RESOURCES.init_with(StackResources::new),
        net_seed,
    );

    spawner.spawn(net_task(runner).expect("failed to spawn the network stack task"));
    spawner.spawn(
        connection_task(wifi_controller).expect("failed to spawn the Wi-Fi connection task"),
    );
}

#[embassy_executor::task]
async fn net_task(
    mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface<'static>>,
) -> ! {
    runner.run().await
}

/// Keeps the station connected using the latest [`WIFI_CREDENTIALS`], reconnecting whenever the
/// connection drops or the credentials change.
#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) -> ! {
    let mut receiver = WIFI_CREDENTIALS
        .receiver()
        .expect("Wi-Fi credentials receiver already taken");

    if let Some(stored) = storage::load::<WifiCredentials>(StorageKey::WifiCredentials).await {
        WIFI_CREDENTIALS.sender().send(stored);
    } else {
        info!("no Wi-Fi credentials stored, waiting for them to be set");
    }

    let mut credentials = receiver.get().await;

    loop {
        info!("connecting to Wi-Fi network {}", credentials.ssid);

        match select(connect(&mut controller, &credentials), receiver.changed()).await {
            Either::First(Ok(())) => {
                info!("connected to Wi-Fi");

                match select(controller.wait_for_disconnect_async(), receiver.changed()).await {
                    Either::First(_) => {
                        warn!("Wi-Fi disconnected");
                        Timer::after(RECONNECT_DELAY).await;
                    }
                    Either::Second(new_credentials) => {
                        credentials = new_credentials;
                        if let Err(e) = controller.disconnect_async().await {
                            warn!("failed to disconnect from Wi-Fi: {}", e);
                        }
                    }
                }
            }
            Either::First(Err(e)) => {
                warn!("failed to connect to Wi-Fi: {}", e);

                if let Either::Second(new_credentials) =
                    select(Timer::after(RECONNECT_DELAY), receiver.changed()).await
                {
                    credentials = new_credentials;
                }
            }
            Either::Second(new_credentials) => credentials = new_credentials,
        }
    }
}

async fn connect(
    controller: &mut WifiController<'static>,
    credentials: &WifiCredentials,
) -> Result<(), WifiError> {
    let auth_method = if credentials.password.is_empty() {
        AuthenticationMethod::None
    } else {
        AuthenticationMethod::Wpa2Personal
    };

    controller.set_config(&Config::Station(
        StationConfig::default()
            .with_ssid(credentials.ssid.as_str())
            .with_password(credentials.password.as_str().into())
            .with_auth_method(auth_method),
    ))?;
    controller.connect_async().await?;

    Ok(())
}
