#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use defmt::info;
use embassy_executor::Spawner;
use esp_hal::clock::CpuClock;
use esp_hal::rmt::Rmt;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use panic_rtt_target as _;

extern crate alloc;

pub mod ble;
pub mod mqtt;
pub mod network;
pub mod sensors;
pub mod servo;
pub mod status_led;
pub mod storage;
pub mod ws2812;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.3.0
    // generator parameters: --chip esp32c6 -o unstable-hal -o alloc -o wifi -o ble-bleps -o embassy -o probe-rs -o defmt -o panic-rtt-target -o zed -o vscode

    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);
    // COEX needs more RAM - so we've added some more
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("Embassy initialized!");

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80))
        .expect("failed to initialize RMT")
        .into_async();

    status_led::init(spawner, rmt.channel0, peripherals.GPIO8);
    servo::init(spawner, rmt.channel1, peripherals.GPIO6);
    sensors::init(spawner, peripherals.ADC1, peripherals.GPIO4, peripherals.GPIO5);
    storage::init(peripherals.FLASH);
    let stack = network::init_network(spawner, peripherals.WIFI);
    mqtt::init(spawner, stack);
    ble::init(spawner, peripherals.BT, peripherals.GPIO9);

    core::future::pending().await
}
