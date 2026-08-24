#![no_std]
#![no_main]
#![allow(static_mut_refs)]

mod hal;

use hal::{Hardware, Leds, UsartTx, UsartRx};
use embassy_time::Timer;

#[embassy_executor::task]
async fn main_task(
    mut leds: Leds,
    mut north_tx: UsartTx,
    mut south_tx: UsartTx,
    mut east_tx: UsartTx,
    mut west_tx: UsartTx,
    mut north_rx: UsartRx,
    mut south_rx: UsartRx,
    mut east_rx: UsartRx,
    mut west_rx: UsartRx,
) {
    loop {
        println!("Tick");
        let _ = leds.write_slice(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF]);
        north_tx.write(b"\0NORTH\0").await;
        Timer::after_millis(10).await;
        south_tx.write(b"\0SOUTH\0").await;
        Timer::after_millis(10).await;
        east_tx.write(b"\0EAST\0").await;
        Timer::after_millis(10).await;
        west_tx.write(b"\0WEST\0").await;
        Timer::after_millis(10).await;

        for (usart_rx, name) in [
            (&mut north_rx, "North"),
            (&mut south_rx, "South"),
            (&mut east_rx, "East"),
            (&mut west_rx, "West"),
        ] {
            let mut available = usart_rx.data_size();
            while available > 0 {
                let len = available.min(16);
                let rx = &mut [0; 16][..len];
                usart_rx.read_slice(rx);
                println!("{} RX {:?} {:?}", name, rx, core::str::from_utf8(rx));
                available = usart_rx.data_size();
            }
        }

        Timer::after_millis(500).await;

        let _ = leds.write_slice(&[0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00]);

        Timer::after_millis(500).await;
    }
}

#[qingke_rt::entry]
fn main() -> ! {
    let Hardware {
        leds,
        mut led_pwr,
        usarts_tx: [north_tx, south_tx, east_tx, west_tx],
        usarts_rx: [north_rx, south_rx, east_rx, west_rx],
    } = Hardware::init();

    led_pwr.set_pwr(true);

    // Create executor
    let executor = embassy_executor::Executor::new();
    let executor = unsafe {
        static mut EXECUTOR: core::mem::MaybeUninit<embassy_executor::Executor> =
            core::mem::MaybeUninit::uninit();
        EXECUTOR.write(executor)
    };

    executor.run(|spawner| {
        spawner.spawn(
            main_task(
                leds, north_tx, south_tx, east_tx, west_tx, north_rx, south_rx, east_rx, west_rx,
            )
            .expect("spawn"),
        );
    });
}
