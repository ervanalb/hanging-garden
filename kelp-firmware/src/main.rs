#![no_std]
#![no_main]

use ch32_hal as hal;
use embassy_time::Timer;
use panic_halt as _;

// Update all 5 LEDs with the same color
// Sends entire message at once with proper reset code
async fn ws2812_update(
    spi: &mut hal::spi::Spi<'_, hal::peripherals::SPI1, hal::mode::Async>,
    r: u8,
    g: u8,
    b: u8,
) {
    // Buffer: 5 LEDs × 3 bytes/LED × 4 SPI bytes per data byte = 60 bytes
    // Reset: 300µs at 4MHz = 1200 bits = 150 bytes of 0x00
    const DATA_BYTES: usize = 5 * 3 * 4; // 60 bytes
    const RESET_BYTES: usize = 150;
    const TOTAL_BYTES: usize = DATA_BYTES + RESET_BYTES; // 210

    let mut buffer = [0u8; TOTAL_BYTES];

    // Encode color data for all 5 LEDs (WS2812 uses GRB order)
    for led in 0..5 {
        let led_offset = led * 3 * 4; // Each LED = 3 color bytes × 4 SPI bytes per color byte
        encode_byte_to_spi(g, &mut buffer, led_offset);
        encode_byte_to_spi(r, &mut buffer, led_offset + 4);
        encode_byte_to_spi(b, &mut buffer, led_offset + 8);
    }

    // Reset code: remaining bytes already initialized to 0x00
    // This provides 150 bytes × 8 bits/byte × 0.25µs/bit = 300µs of low signal

    // Send entire message at once
    spi.write(&buffer).await.ok();
}

// Encode a single byte into WS2812 SPI format
// WS2812 protocol: 0 bit = ~400ns high + ~850ns low, 1 bit = ~800ns high + ~450ns low
// At 4 MHz SPI (250ns per bit), we encode using 4 SPI bits per WS2812 bit:
// '0' as 0b1000 (1 high, 3 low = 250ns high, 750ns low)
// '1' as 0b1110 (3 high, 1 low = 750ns high, 250ns low)
// Each data byte (8 bits) becomes 32 SPI bits = 4 bytes (2 WS2812 bits per SPI byte)
fn encode_byte_to_spi(byte: u8, buffer: &mut [u8], offset: usize) {
    // Process 2 WS2812 bits at a time (fits in 1 SPI byte = 8 bits)
    for i in 0..4 {
        let bit_pair_index = i * 2;
        let bit0 = (byte >> (7 - bit_pair_index)) & 1;
        let bit1 = (byte >> (6 - bit_pair_index)) & 1;

        let pattern0 = if bit0 == 1 { 0b1110 } else { 0b1000 };
        let pattern1 = if bit1 == 1 { 0b1110 } else { 0b1000 };

        buffer[offset + i] = (pattern0 << 4) | pattern1;
    }
}

#[embassy_executor::main(entry = "ch32_hal::entry")]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let p = hal::init(Default::default());

    // PB3 = LED power enable (active low)
    let _led_enable =
        hal::gpio::OutputOpenDrain::new(p.PB3, hal::gpio::Level::Low, Default::default());

    // Configure SPI1 for WS2812 control on PB5 (MOSI)
    // SPI frequency: aim for ~6.4 MHz for WS2812 timing
    // With 8 MHz system clock, divider of 2 gives 4 MHz (close enough)
    let mut spi_config = hal::spi::Config::default();
    spi_config.frequency = hal::time::Hertz(4_000_000);
    let mut spi = hal::spi::Spi::new_txonly_nosck(p.SPI1, p.PB5, p.DMA1_CH3, spi_config);

    loop {
        ws2812_update(&mut spi, 5, 0, 0).await;
        Timer::after_millis(500).await;
        ws2812_update(&mut spi, 0, 5, 0).await;
        Timer::after_millis(500).await;
        ws2812_update(&mut spi, 0, 0, 5).await;
        Timer::after_millis(500).await;
    }
}
