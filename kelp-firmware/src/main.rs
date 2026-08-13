#![no_std]
#![no_main]

use ch32_hal as hal;
use embassy_time::Timer;
use hal::adc::{ADC_MAX, Pga, SampleTime, VREF_INT, VrefInt};
use hal::touchkey::{ChargeTime, TouchKey};
use panic_halt as _;

const TOUCHKEY_DISCHARGE_TIME: u8 = 0x08;

const LED_COUNT: usize = 50;

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
    const DATA_BYTES: usize = LED_COUNT * 3 * 4; // 60 bytes
    const RESET_BYTES: usize = 150;
    const TOTAL_BYTES: usize = DATA_BYTES + RESET_BYTES; // 210

    let mut buffer = [0u8; TOTAL_BYTES];

    // Encode color data for all 5 LEDs (WS2812 uses GRB order)
    for led in 0..LED_COUNT {
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

/*
#[embassy_executor::main(entry = "ch32_hal::entry")]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let p = hal::init(Default::default());

    hal::debug::SDIPrint::enable();

    // Configure ADC1 for regular ADC readings
    let mut adc = hal::adc::Adc::new(p.ADC1, Default::default());
    let mut pa6 = p.PA6;
    let mut pa7 = p.PA7;
    let mut vrefint = VrefInt;

    // Configure ADC2 for touchkey
    let mut touchkey = TouchKey::new(p.ADC2, Default::default());
    let mut pa1 = p.PA1;

    // Touchkey baseline
    let mut touchkey_baseline: Option<u16> = None;

    // PB3 = LED power enable (active low)
    let _led_enable =
        hal::gpio::OutputOpenDrain::new(p.PB3, hal::gpio::Level::Low, Default::default());

    // Configure SPI1 for WS2812 control on PB5 (MOSI)
    // SPI frequency: aim for ~6.4 MHz for WS2812 timing
    // With 8 MHz system clock, divider of 2 gives 4 MHz (close enough)
    let mut spi_config = hal::spi::Config::default();
    spi_config.frequency = hal::time::Hertz(4_000_000);
    let mut spi = hal::spi::Spi::new_txonly_nosck(p.SPI1, p.PB5, p.DMA1_CH3, spi_config);

    let mut cnt = 0;

    loop {
        use hal::println;

        // Read touchkey from PA1
        let touchkey_value = touchkey.read(&mut pa1, ChargeTime::CYCLES7_5, TOUCHKEY_DISCHARGE_TIME);

        // Establish touchkey baseline on first reading
        if touchkey_baseline.is_none() {
            touchkey_baseline = Some(touchkey_value);
        }

        // Calculate touchkey difference from baseline
        let touchkey_diff = if let Some(baseline) = touchkey_baseline {
            baseline.saturating_sub(touchkey_value)
        } else {
            0
        };

        // Read internal voltage reference
        let vref_reading = adc.convert(&mut vrefint, SampleTime::CYCLES239_5, Pga::X1);

        // Calculate actual VDDA in millivolts
        let vdda_mv = (VREF_INT * ADC_MAX) / vref_reading as u32;

        let pa6_value = adc.convert(&mut pa6, SampleTime::CYCLES239_5, Pga::X1);
        let pa7_value = adc.convert(&mut pa7, SampleTime::CYCLES239_5, Pga::X1);

        // Convert PA6 ADC value to voltage in millivolts
        let pa6_voltage_mv = (pa6_value as u32 * vdda_mv) / ADC_MAX;

        // Calculate current through 50mOhm sense resistor
        // I = V / R, where R = 0.05 Ohm
        // Current in A = (voltage_mv / 1000) / 0.05 = voltage_mv / 50
        let current_ma = (pa6_voltage_mv * 1000) / 50; // Current in milliamps
        let current_a = current_ma / 1000; // Current in amps (integer part)
        let current_ma_frac = current_ma % 1000; // Fractional part in milliamps

        // Convert PA7 ADC value to actual voltage in millivolts
        let pa7_voltage_mv = (pa7_value as u32 * vdda_mv) / ADC_MAX;

        // PA7 reads half of bus voltage, so multiply by 2
        let bus_voltage_mv = pa7_voltage_mv * 2;

        println!(
            "Touch: {} (diff: {}), VDDA: {} mV, Current: {}.{} A, Bus: {} mV",
            touchkey_value, touchkey_diff, vdda_mv, current_a, current_ma_frac, bus_voltage_mv
        );

        Timer::after_millis(50).await;

        if cnt < 10 {
            ws2812_update(&mut spi, 255, 255, 255).await;
            cnt += 1;
        } else if cnt < 20 {
            ws2812_update(&mut spi, 0, 0, 0).await;
            cnt += 1;
        } else {
            cnt = 0;
        }
    }
}
*/

// New USART test main with DMA
#[embassy_executor::main(entry = "ch32_hal::entry")]
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let p = hal::init(Default::default());

    hal::debug::SDIPrint::enable();

    use hal::println;
    println!("USART Half-Duplex DMA Test Starting");

    // Configure all 4 USARTs in async half-duplex mode at 115.2 kbps with DMA
    let mut config = hal::usart::Config::default();
    config.baudrate = 115_200;

    // USART1 - South (PA9) with DMA1_CH4 (TX) and DMA1_CH5 (RX)
    println!("Init USART1 (South) on PA9 with DMA");
    let south =
        hal::usart::Uart::new_half_duplex::<0>(p.USART1, p.PA9, p.DMA1_CH4, p.DMA1_CH5, config)
            .unwrap();
    let (mut south_tx, mut south_rx) = south.split();

    // USART2 - West (PA2) with DMA1_CH7 (TX) and DMA1_CH6 (RX)
    println!("Init USART2 (West) on PA2 with DMA");
    let west =
        hal::usart::Uart::new_half_duplex::<0>(p.USART2, p.PA2, p.DMA1_CH7, p.DMA1_CH6, config)
            .unwrap();
    let (mut west_tx, mut west_rx) = west.split();

    // USART3 - North (PB10) with DMA1_CH2 (TX) and DMA1_CH3 (RX)
    println!("Init USART3 (North) on PB10 with DMA");
    let north =
        hal::usart::Uart::new_half_duplex::<0>(p.USART3, p.PB10, p.DMA1_CH2, p.DMA1_CH3, config)
            .unwrap();
    let (mut north_tx, mut north_rx) = north.split();

    // USART4 - East (PB0 with REMAP=0) with DMA1_CH1 (TX) and DMA1_CH8 (RX)
    println!("Init USART4 (East) on PB0 with DMA");
    let east =
        hal::usart::Uart::new_half_duplex::<1>(p.USART4, p.PB0, p.DMA1_CH1, p.DMA1_CH8, config)
            .unwrap();
    // Manually force USART4 remap to 0 to use PB0/PB1 pins (D6 variant mapping)
    unsafe {
        hal::pac::AFIO.pcfr2().modify(|w| w.set_usart4_rm(0));
    }
    let (mut east_tx, mut east_rx) = east.split();

    println!("All USARTs configured with DMA successfully");

    let mut south_rx_buf = [0u8; 3];
    let mut west_rx_buf = [0u8; 3];
    let mut north_rx_buf = [0u8; 3];
    let mut east_rx_buf = [0u8; 3];
    let mut iteration = 0u32;

    loop {
        iteration = iteration.wrapping_add(1);

        // Test South (USART1) - Concurrently send and read from other ports
        {
            use embassy_futures::join::join4;

            let send = async {
                south_tx.write(b"SOUTH").await.ok();
                println!("[{}] Sent from SOUTH via DMA", iteration);
            };

            let read_west = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    west_rx.read(&mut west_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> WEST received bytes: {:?}", &west_rx_buf);
                    }
                    _ => {}
                }
            };

            let read_north = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    north_rx.read(&mut north_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> NORTH received bytes: {:?}", &north_rx_buf);
                    }
                    _ => {}
                }
            };

            let read_east = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    east_rx.read(&mut east_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> EAST received bytes: {:?}", &east_rx_buf);
                    }
                    _ => {}
                }
            };

            join4(send, read_west, read_north, read_east).await;
        }

        // Test West (USART2) - Concurrently send and read from other ports
        {
            use embassy_futures::join::join4;

            let send = async {
                west_tx.write(b"WEST").await.ok();
                println!("[{}] Sent from WEST via DMA", iteration);
            };

            let read_south = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    south_rx.read(&mut south_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> SOUTH received bytes: {:?}", &south_rx_buf);
                    }
                    _ => {}
                }
            };

            let read_north = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    north_rx.read(&mut north_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> NORTH received bytes: {:?}", &north_rx_buf);
                    }
                    _ => {}
                }
            };

            let read_east = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    east_rx.read(&mut east_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> EAST received bytes: {:?}", &east_rx_buf);
                    }
                    _ => {}
                }
            };

            join4(send, read_south, read_north, read_east).await;
        }

        // Test North (USART3) - Concurrently send and read from other ports
        {
            use embassy_futures::join::join4;

            let send = async {
                north_tx.write(b"NORTH").await.ok();
                println!("[{}] Sent from NORTH via DMA", iteration);
            };

            let read_south = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    south_rx.read(&mut south_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> SOUTH received bytes: {:?}", &south_rx_buf);
                    }
                    _ => {}
                }
            };

            let read_west = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    west_rx.read(&mut west_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> WEST received bytes: {:?}", &west_rx_buf);
                    }
                    _ => {}
                }
            };

            let read_east = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    east_rx.read(&mut east_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> EAST received bytes: {:?}", &east_rx_buf);
                    }
                    _ => {}
                }
            };

            join4(send, read_south, read_west, read_east).await;
        }

        // Test East (USART4) - Concurrently send and read from other ports
        {
            use embassy_futures::join::join4;

            let send = async {
                east_tx.write(b"EAST").await.ok();
                println!("[{}] Sent from EAST via DMA", iteration);
            };

            let read_south = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    south_rx.read(&mut south_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> SOUTH received bytes: {:?}", &south_rx_buf);
                    }
                    _ => {}
                }
            };

            let read_west = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    west_rx.read(&mut west_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> WEST received bytes: {:?}", &west_rx_buf);
                    }
                    _ => {}
                }
            };

            let read_north = async {
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(10),
                    north_rx.read(&mut north_rx_buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        println!("  -> NORTH received bytes: {:?}", &north_rx_buf);
                    }
                    _ => {}
                }
            };

            join4(send, read_south, read_west, read_north).await;
        }

        // Longer pause after testing all 4 directions
        println!("--- Cycle {} complete, pausing ---\n", iteration);
        Timer::after_millis(1000).await;
    }
}

/*
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
*/
