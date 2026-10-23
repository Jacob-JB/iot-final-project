use esp_hal::{
    Async, gpio,
    rmt::{self, PulseCode, TxChannelCreator},
};

pub struct Ws2812Writer<'d> {
    rmt_channel: rmt::Channel<'d, Async, rmt::Tx>,
}

impl<'d> Ws2812Writer<'d> {
    /// Creates a new writer with a rmt channel and gpio pin.
    /// Assumes the RMT clock is 80 MHz
    pub fn new(
        rmt_channel: impl TxChannelCreator<'d, Async>,
        pin: impl gpio::interconnect::PeripheralOutput<'d>,
    ) -> Self {
        let channel = rmt_channel
            .configure_tx(&rmt::TxChannelConfig::default().with_clk_divider(1))
            .unwrap()
            .with_pin(pin);

        Ws2812Writer {
            rmt_channel: channel,
        }
    }

    /// Encode one WS2812 GRB color into RMT pulse codes (24 data bits + end marker) and transmit.
    ///
    /// Timings are the standard WS2812B datasheet values, expressed in ticks of
    /// an 80 MHz RMT clock (1 tick = 12.5 ns): bit 0 is 400 ns high / 850 ns low,
    /// bit 1 is 800 ns high / 450 ns low.
    pub async fn write(&mut self, red: u8, green: u8, blue: u8) {
        let mut pulses = [PulseCode::end_marker(); 25];

        let grb = ((green as u32) << 16) | ((red as u32) << 8) | (blue as u32);

        for (i, pulse) in pulses.iter_mut().take(24).enumerate() {
            let bit = (grb >> (23 - i)) & 1 != 0;
            *pulse = match bit {
                true => PulseCode::new(gpio::Level::High, 64, gpio::Level::Low, 36),
                false => PulseCode::new(gpio::Level::High, 32, gpio::Level::Low, 68),
            };
        }

        let _ = self.rmt_channel.transmit(&pulses).await;
    }
}
