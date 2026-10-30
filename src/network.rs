use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::{Stack, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use embassy_time::{Duration, Timer};
use esp_radio::wifi::{
    AuthenticationMethod, Config, WifiController, WifiError, sta::StationConfig,
};
use sequential_storage::map::{SerializationError, Value};
use static_cell::StaticCell;

use crate::{
    status_led::{self, WifiStatus},
    storage::{self, StorageError, StorageKey, deserialize_str, serialize_str},
};

static NETWORK_STACK_RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();

/// The credentials the station connects with. Empty until [`connection_task`] loads them from
/// flash, and `None` while the station should stay disconnected.
static WIFI_CREDENTIALS: Watch<CriticalSectionRawMutex, Option<WifiCredentials>, 1> = Watch::new();

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

/// Saves `credentials` to flash, then reconnects the station with them.
///
/// The station keeps its current credentials if saving fails.
pub async fn set_wifi_credentials(credentials: WifiCredentials) -> Result<(), StorageError> {
    storage::store(StorageKey::WifiCredentials, &credentials).await?;
    WIFI_CREDENTIALS.sender().send(Some(credentials));
    Ok(())
}

/// Removes the credentials from flash, then disconnects the station.
///
/// The station keeps its current credentials if removing them fails.
pub async fn clear_wifi_credentials() -> Result<(), StorageError> {
    storage::remove(StorageKey::WifiCredentials).await?;
    WIFI_CREDENTIALS.sender().send(None);
    Ok(())
}

pub fn wifi_credentials() -> Option<WifiCredentials> {
    WIFI_CREDENTIALS.try_get().flatten()
}

/// Starts the network stack and keeps the station connected, returning the stack.
///
/// Requires [`storage::init`] to have been called.
pub fn init_network(
    spawner: Spawner,
    wifi_peripheral: esp_hal::peripherals::WIFI<'static>,
) -> Stack<'static> {
    let rng = esp_hal::rng::Rng::new();

    let (wifi_controller, interfaces) = esp_radio::wifi::new(wifi_peripheral, Default::default())
        .expect("Failed to initialize Wi-Fi controller");

    let net_seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        NETWORK_STACK_RESOURCES.init_with(StackResources::new),
        net_seed,
    );

    spawner.spawn(net_task(runner).expect("failed to spawn the network stack task"));
    spawner.spawn(
        connection_task(wifi_controller).expect("failed to spawn the Wi-Fi connection task"),
    );

    stack
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

    let stored = storage::load::<WifiCredentials>(StorageKey::WifiCredentials).await;
    WIFI_CREDENTIALS.sender().send(stored);

    let mut credentials = receiver.get().await;

    loop {
        let Some(current_credentials) = &credentials else {
            info!("no Wi-Fi credentials set, waiting for them to be set");
            set_wifi_status(WifiStatus::Unconfigured);

            if controller.is_connected()
                && let Err(e) = controller.disconnect_async().await
            {
                warn!("failed to disconnect from Wi-Fi: {}", e);
            }

            credentials = receiver.changed().await;
            continue;
        };

        info!("connecting to Wi-Fi network {}", current_credentials.ssid);
        set_wifi_status(WifiStatus::Connecting);

        match select(
            connect(&mut controller, current_credentials),
            receiver.changed(),
        )
        .await
        {
            Either::First(Ok(())) => {
                info!("connected to Wi-Fi");
                set_wifi_status(WifiStatus::Connected);

                match select(controller.wait_for_disconnect_async(), receiver.changed()).await {
                    Either::First(_) => {
                        warn!("Wi-Fi disconnected");
                        set_wifi_status(WifiStatus::Connecting);
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

fn set_wifi_status(wifi: WifiStatus) {
    status_led::update_status(|status| status.wifi = wifi);
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
