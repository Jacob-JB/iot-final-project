use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    watch::{Receiver, Watch},
};
use embassy_time::{Duration, Timer};
use esp_hal::{Async, gpio::interconnect::PeripheralOutput, rmt::TxChannelCreator};

use crate::ws2812::Ws2812Writer;

static STATUS: Watch<CriticalSectionRawMutex, DeviceStatus, 1> = Watch::new_with(DeviceStatus {
    ble: BleStatus::Disabled,
    wifi: WifiStatus::Unconfigured,
});

/// Status reported by the parts of the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub struct DeviceStatus {
    pub ble: BleStatus,
    pub wifi: WifiStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum BleStatus {
    Disabled,
    Advertising,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum WifiStatus {
    /// No credentials are set.
    Unconfigured,
    Connecting,
    Connected,
}

/// Applies `f` to the device status. The LED only updates if the status changed.
pub fn update_status(f: impl Fn(&mut DeviceStatus)) {
    STATUS.sender().send_if_modified(|status| {
        let status = status.as_mut().expect("device status is always set");

        let old = *status;
        f(status);
        *status != old
    });
}

pub fn status() -> DeviceStatus {
    STATUS.try_get().expect("device status is always set")
}

/// Starts showing the [`DeviceStatus`] on the WS2812 LED connected to `pin`.
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
    let led = Ws2812Writer::new(rmt_channel, pin);
    let receiver = STATUS
        .receiver()
        .expect("status LED initialized more than once");

    spawner.spawn(led_task(led, receiver).expect("failed to spawn the status LED task"));
}

#[embassy_executor::task]
async fn led_task(
    mut led: Ws2812Writer<'static>,
    mut receiver: Receiver<'static, CriticalSectionRawMutex, DeviceStatus, 1>,
) -> ! {
    let mut status = receiver.get().await;

    loop {
        let Either::Second(new_status) = select(show(&mut led, status), receiver.changed()).await;

        status = new_status;
    }
}

/// Displays `status` on the LED until cancelled. Restarted whenever the status changes.
async fn show(led: &mut Ws2812Writer<'static>, status: DeviceStatus) -> ! {
    let mut flash = async |r, g, b, time| {
        led.write(r, g, b).await;
        Timer::after(Duration::from_millis(time)).await;
    };

    loop {
        match status.ble {
            BleStatus::Disabled => (),
            BleStatus::Advertising => flash(0, 0, 10, 500).await,
            BleStatus::Connected => flash(10, 0, 10, 500).await,
        }

        match status.wifi {
            WifiStatus::Unconfigured => (),
            WifiStatus::Connecting => flash(10, 10, 0, 500).await,
            WifiStatus::Connected => flash(0, 10, 0, 500).await,
        };

        flash(0, 0, 0, 2000).await;
    }
}
