use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::gpio::{Input, InputConfig, InputPin, Pull};
use esp_radio::ble::controller::{BleConnector, BleConnectorError};
use trouble_host::prelude::*;

use crate::{
    network::{self, WifiCredentials},
    status_led::{self, BleStatus},
};

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 3;
const CONTROLLER_SLOTS: usize = 20;

const DEVICE_NAME: &str = "smart-blind";

/// Locally administered random static address, see the Bluetooth Core Spec, Vol 6, Part B, 1.3.2.
const ADDRESS: [u8; 6] = [0xC0, 0x45, 0x44, 0x43, 0x42, 0xC1];

/// How long the server advertises without a connection before it is disabled.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the button must stay pressed to count as a press.
const DEBOUNCE_TIME: Duration = Duration::from_millis(50);

type BleController = ExternalController<BleConnector<'static>, CONTROLLER_SLOTS>;
type BleError = BleHostError<BleConnectorError>;

#[gatt_service(uuid = "5c0b1a10-6f2e-4b8e-9d3a-7e1f2a3b4c00")]
struct WifiService {
    /// Writing an empty SSID forgets the credentials and disconnects the station.
    #[descriptor(uuid = descriptors::CHARACTERISTIC_USER_DESCRIPTION, read, value = "SSID")]
    #[characteristic(uuid = "5c0b1a10-6f2e-4b8e-9d3a-7e1f2a3b4c01", read, write)]
    ssid: heapless::String<32>,

    /// Writing the password applies it together with the current [`WifiService::ssid`].
    /// Write an empty password for an open network.
    #[descriptor(uuid = descriptors::CHARACTERISTIC_USER_DESCRIPTION, read, value = "Password")]
    #[characteristic(uuid = "5c0b1a10-6f2e-4b8e-9d3a-7e1f2a3b4c02", write)]
    password: heapless::String<64>,
}

#[gatt_server]
struct Server {
    wifi_service: WifiService,
}

/// Starts the BLE configuration server task. Pressing `button` toggles the server, which also
/// disables itself after [`INACTIVITY_TIMEOUT`] without a connection.
///
/// `button` must be active low.
///
/// # Panics
///
/// Panics if the BLE controller fails to initialize.
pub fn init(
    spawner: Spawner,
    bt: esp_hal::peripherals::BT<'static>,
    button: impl InputPin + 'static,
) {
    let connector =
        BleConnector::new(bt, Default::default()).expect("failed to initialize the BLE controller");
    let button = Input::new(button, InputConfig::default().with_pull(Pull::Up));

    spawner.spawn(ble_task(connector, button).expect("failed to spawn the BLE task"));
}

#[embassy_executor::task]
async fn ble_task(connector: BleConnector<'static>, mut button: Input<'static>) -> ! {
    let controller: BleController = ExternalController::new(connector);
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack =
        trouble_host::new(controller, &mut resources).set_random_address(Address::random(ADDRESS));
    let Host {
        mut peripheral,
        mut runner,
        ..
    } = stack.build();

    let server =
        Server::new_default(DEVICE_NAME).expect("device name too long for the GAP service");

    let app = async {
        loop {
            wait_for_press(&mut button).await;

            info!("BLE enabled");

            if let Err(e) = serve(&mut peripheral, &server, &mut button).await {
                warn!("BLE error: {}", e);
            }

            info!("BLE disabled");
            set_ble_status(BleStatus::Disabled);
        }
    };

    let Either::First(result) = select(runner.run(), app).await;
    defmt::panic!("BLE host stopped: {}", result);
}

fn set_ble_status(ble: BleStatus) {
    status_led::update_status(|status| status.ble = ble);
}

/// Waits for `button` to be released and then pressed again.
async fn wait_for_press(button: &mut Input<'_>) {
    button.wait_for_high().await;

    loop {
        button.wait_for_low().await;
        Timer::after(DEBOUNCE_TIME).await;

        if button.is_low() {
            return;
        }
    }
}

/// Advertises and serves connections until `button` is pressed or [`INACTIVITY_TIMEOUT`] passes
/// without a connection.
async fn serve(
    peripheral: &mut Peripheral<'_, BleController, DefaultPacketPool>,
    server: &Server<'_>,
    button: &mut Input<'_>,
) -> Result<(), BleError> {
    let ssid = network::wifi_credentials()
        .map(|credentials| credentials.ssid)
        .unwrap_or_default();
    server.set(&server.wifi_service.ssid, &ssid)?;

    loop {
        set_ble_status(BleStatus::Advertising);
        let advertiser = advertise(peripheral).await?;

        let connection = match select(
            with_timeout(INACTIVITY_TIMEOUT, advertiser.accept()),
            wait_for_press(button),
        )
        .await
        {
            Either::First(Ok(connection)) => connection?,
            Either::First(Err(_)) => {
                info!("no BLE connection before the inactivity timeout");
                return Ok(());
            }
            Either::Second(()) => return Ok(()),
        };
        let connection = connection.with_attribute_server(&server.server)?;

        info!("BLE connected");
        set_ble_status(BleStatus::Connected);

        if let ConnectionEnd::ButtonPressed = serve_connection(server, &connection, button).await {
            connection.raw().disconnect();
            return Ok(());
        }
    }
}

async fn advertise<'stack>(
    peripheral: &mut Peripheral<'stack, BleController, DefaultPacketPool>,
) -> Result<Advertiser<'stack, BleController, DefaultPacketPool>, BleError> {
    let mut adv_data = [0; 31];
    let adv_data_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(DEVICE_NAME.as_bytes()),
        ],
        &mut adv_data[..],
    )?;

    info!("advertising as \"{}\"", DEVICE_NAME);

    peripheral
        .advertise(
            &AdvertisementParameters::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv_data[..adv_data_len],
                scan_data: &[],
            },
        )
        .await
}

enum ConnectionEnd {
    Disconnected,
    ButtonPressed,
}

/// Handles GATT requests until the client disconnects or `button` is pressed. The connection is
/// still open when this returns [`ConnectionEnd::ButtonPressed`].
async fn serve_connection(
    server: &Server<'_>,
    connection: &GattConnection<'_, '_, DefaultPacketPool>,
    button: &mut Input<'_>,
) -> ConnectionEnd {
    loop {
        let event = match select(connection.next(), wait_for_press(button)).await {
            Either::First(event) => event,
            Either::Second(()) => return ConnectionEnd::ButtonPressed,
        };

        match event {
            GattConnectionEvent::Disconnected { reason } => {
                info!("BLE disconnected: {}", reason);
                return ConnectionEnd::Disconnected;
            }
            GattConnectionEvent::Gatt { event } => {
                let reply = match &event {
                    GattEvent::Write(write)
                        if write.handle() == server.wifi_service.ssid.handle =>
                    {
                        match parse_str::<32>(write.data()) {
                            Some(ssid) if ssid.is_empty() => match forget_credentials().await {
                                Ok(()) => event.accept(),
                                Err(code) => event.reject(code),
                            },
                            Some(_) => event.accept(),
                            None => event.reject(AttErrorCode::VALUE_NOT_ALLOWED),
                        }
                    }
                    GattEvent::Write(write)
                        if write.handle() == server.wifi_service.password.handle =>
                    {
                        match apply_credentials(server, write.data()).await {
                            Ok(()) => event.accept(),
                            Err(code) => event.reject(code),
                        }
                    }
                    _ => event.accept(),
                };

                match reply {
                    Ok(reply) => reply.send().await,
                    Err(e) => warn!("failed to reply to GATT request: {}", e),
                }
            }
            _ => {}
        }
    }
}

/// Saves the written `password` together with the SSID characteristic's value as the Wi-Fi
/// credentials.
async fn apply_credentials(server: &Server<'_>, password: &[u8]) -> Result<(), AttErrorCode> {
    let password = parse_str(password).ok_or(AttErrorCode::VALUE_NOT_ALLOWED)?;
    let ssid = server
        .get(&server.wifi_service.ssid)
        .map_err(|_| AttErrorCode::UNLIKELY_ERROR)?;

    if ssid.is_empty() {
        warn!("rejected Wi-Fi password because no SSID is set");
        return Err(AttErrorCode::VALUE_NOT_ALLOWED);
    }

    info!("Wi-Fi credentials set over BLE for {}", ssid);

    network::set_wifi_credentials(WifiCredentials { ssid, password })
        .await
        .map_err(|e| {
            warn!("failed to save Wi-Fi credentials: {}", e);
            AttErrorCode::UNLIKELY_ERROR
        })
}

async fn forget_credentials() -> Result<(), AttErrorCode> {
    info!("Wi-Fi credentials cleared over BLE");

    network::clear_wifi_credentials().await.map_err(|e| {
        warn!("failed to clear Wi-Fi credentials: {}", e);
        AttErrorCode::UNLIKELY_ERROR
    })
}

/// Returns `None` if `bytes` is not UTF-8 or is longer than `N` bytes.
fn parse_str<const N: usize>(bytes: &[u8]) -> Option<heapless::String<N>> {
    let s = core::str::from_utf8(bytes).ok()?;
    heapless::String::try_from(s).ok()
}
