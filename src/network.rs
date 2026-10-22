use embassy_executor::Spawner;
use embassy_net::StackResources;
use static_cell::StaticCell;

static NETWORK_STACK_RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();

pub fn init_network(spawner: Spawner, wifi_peripheral: esp_hal::peripherals::WIFI<'static>) {
    let rng = esp_hal::rng::Rng::new();

    let (mut wifi_controller, interfaces) =
        esp_radio::wifi::new(wifi_peripheral, Default::default())
            .expect("Failed to initialize Wi-Fi controller");

    let net_seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        NETWORK_STACK_RESOURCES.init(StackResources::new()),
        net_seed,
    );

    spawner.spawn(net_task(runner).expect("failed to spawn the network stack task"));
}

#[embassy_executor::task]
async fn net_task(
    mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface<'static>>,
) -> ! {
    runner.run().await
}
