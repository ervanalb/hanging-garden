#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_stm32::usart::{Config as UsartConfig, Uart};
use embassy_stm32::usb::{Driver, Instance};
use embassy_stm32::{bind_interrupts, peripherals, usb};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use embassy_usb::Builder;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USB => usb::InterruptHandler<peripherals::USB>;
    USART1 => embassy_stm32::usart::InterruptHandler<peripherals::USART1>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());
    info!("USB-UART Bridge starting...");

    // Create USB driver
    let driver = Driver::new(p.USB, Irqs, p.PA12, p.PA11);

    // Create embassy-usb Config
    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Hanging Garden");
    config.product = Some("USB-UART Bridge");
    config.serial_number = Some("12345678");

    // Create embassy-usb DeviceBuilder
    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut control_buf = [0; 64];
    let mut state = State::new();

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [],
        &mut control_buf,
    );

    // Create CDC-ACM class
    let mut cdc_class = CdcAcmClass::new(&mut builder, &mut state, 64);

    // Build the USB device
    let mut usb = builder.build();

    // Configure USART1 for half-duplex at 1 MBaud
    let mut uart_config = UsartConfig::default();
    uart_config.baudrate = 1_000_000; // 1 MBaud
    let mut uart = Uart::new_half_duplex(
        p.USART1,
        p.PB6, // TX pin (also used for RX in half-duplex)
        Irqs,
        p.DMA1_CH2,
        p.DMA1_CH3,
        uart_config,
    )
    .unwrap();

    info!("USB and UART initialized");

    // Run USB device
    let usb_fut = usb.run();

    // Bidirectional bridge task
    let bridge_fut = async {
        loop {
            cdc_class.wait_connection().await;
            info!("USB Connected");
            let _ = bidirectional_bridge(&mut cdc_class, &mut uart).await;
            info!("USB Disconnected");
        }
    };

    // Run both tasks concurrently
    embassy_futures::join::join(usb_fut, bridge_fut).await;
}

struct Disconnected {}

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => {
                warn!("USB Buffer overflow");
                Disconnected {}
            }
            EndpointError::Disabled => Disconnected {},
        }
    }
}

// Bidirectional bridge between USB and UART
async fn bidirectional_bridge<'d, T: Instance + 'd>(
    class: &mut CdcAcmClass<'d, Driver<'d, T>>,
    uart: &mut Uart<'d, peripherals::USART1, peripherals::DMA1_CH2, peripherals::DMA1_CH3>,
) -> Result<(), Disconnected> {
    let mut usb_buf = [0; 64];
    let mut uart_buf = [0; 64];

    loop {
        // Use select to handle both directions concurrently
        match select(
            class.read_packet(&mut usb_buf),
            uart.read_until_idle(&mut uart_buf),
        )
        .await
        {
            Either::First(result) => {
                // Data received from USB, send to UART
                let n = result?;
                let data = &usb_buf[..n];
                if let Err(_e) = uart.write(data).await {
                    warn!("UART write error");
                }
            }
            Either::Second(result) => {
                // Data received from UART, send to USB
                if let Ok(n) = result {
                    let data = &uart_buf[..n];
                    if let Err(_e) = class.write_packet(data).await {
                        warn!("USB write error");
                    }
                } else {
                    warn!("UART read error");
                }
            }
        }
    }
}
