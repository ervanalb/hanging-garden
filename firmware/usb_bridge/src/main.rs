#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_stm32::usart::{Config as UsartConfig, RingBufferedUartRx, UartTx};
use embassy_stm32::usb::{Driver, Instance};
use embassy_stm32::{bind_interrupts, peripherals, usb};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State};
use embassy_usb::Builder;
extern crate panic_halt;

bind_interrupts!(struct Irqs {
    USB => usb::InterruptHandler<peripherals::USB>;
    USART1 => embassy_stm32::usart::InterruptHandler<peripherals::USART1>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());

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
    let cdc_class = CdcAcmClass::new(&mut builder, &mut state, 64);

    // Build the USB device
    let mut usb = builder.build();

    // Configure USART1 for half-duplex at 1 MBaud with ring-buffered RX
    let mut uart_config = UsartConfig::default();
    uart_config.baudrate = 1_000_000; // 1 MBaud
    let uart = embassy_stm32::usart::Uart::new_half_duplex(
        p.USART1,
        p.PB6, // TX pin (also used for RX in half-duplex)
        Irqs,
        p.DMA1_CH2,
        p.DMA1_CH3,
        uart_config,
    )
    .unwrap();

    // Split UART into TX and RX parts for ring buffering
    let (mut uart_tx, uart_rx) = uart.split();

    // Create ring buffer for RX (256 byte buffer)
    static mut UART_RX_RING_BUF: [u8; 256] = [0u8; 256];
    let mut uart_rx = uart_rx.into_ring_buffered(unsafe { &mut UART_RX_RING_BUF });

    // Split USB CDC class into RX and TX
    let (mut usb_tx, mut usb_rx) = cdc_class.split();

    // Run USB device
    let usb_fut = usb.run();

    // Bidirectional bridge task
    let bridge_fut = async {
        loop {
            usb_rx.wait_connection().await;
            bidirectional_bridge(&mut usb_tx, &mut usb_rx, &mut uart_tx, &mut uart_rx).await;
        }
    };

    // Run both tasks concurrently
    embassy_futures::join::join(usb_fut, bridge_fut).await;
}

// Bidirectional bridge between USB and UART
async fn bidirectional_bridge<'d, T: Instance + 'd>(
    usb_tx: &mut Sender<'d, Driver<'d, T>>,
    usb_rx: &mut Receiver<'d, Driver<'d, T>>,
    uart_tx: &mut UartTx<'d, peripherals::USART1, peripherals::DMA1_CH2>,
    uart_rx: &mut RingBufferedUartRx<'d, peripherals::USART1, peripherals::DMA1_CH3>,
) {
    // USB -> UART direction
    let usb_to_uart = async {
        let mut buf = [0; 64];
        loop {
            let Ok(n) = usb_rx.read_packet(&mut buf).await else {
                break;
            };
            let data = &buf[..n];
            if let Err(_e) = uart_tx.write(data).await {}
        }
    };

    // UART -> USB direction
    let uart_to_usb = async {
        let mut buf = [0; 64];
        loop {
            if let Ok(n) = uart_rx.read(&mut buf).await {
                let data = &buf[..n];
                if let Err(_e) = usb_tx.write_packet(data).await {}
            }
        }
    };

    // Run both directions concurrently
    embassy_futures::join::join(usb_to_uart, uart_to_usb).await;
}
