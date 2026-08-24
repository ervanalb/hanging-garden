#![no_std]
#![no_main]
#![allow(static_mut_refs)]

mod hal;

use embassy_time::Timer;
use hal::{Hardware, Leds, USART_RX_CAPACITY, UsartRx, UsartTx};

impl UsartRx {
    async fn read_until_zero(
        &mut self,
    ) -> fring::Region<'_, fring::Consumer<'static, u8, USART_RX_CAPACITY>, u8> {
        let region = self.read(usize::MAX).await;
        let mut null_index = region.len() - 1;
        for i in 0..region.len() {
            if region[i] == 0 {
                null_index = i;
                break;
            }
        }
        core::mem::forget(region);
        self.read_nb(null_index + 1)
    }
}

#[embassy_executor::task]
async fn main_task(
    mut leds: Leds,
    mut north_tx: UsartTx,
    mut south_tx: UsartTx,
    mut east_tx: UsartTx,
    mut west_tx: UsartTx,
) {
    loop {
        //println!("Tick");
        let _ = leds.write_slice(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF]);
        north_tx.write(b"\0NORTH\0").await;
        Timer::after_millis(10).await;
        south_tx.write(b"\0SOUTH\0").await;
        Timer::after_millis(10).await;
        east_tx.write(b"\0EAST\0").await;
        Timer::after_millis(10).await;
        west_tx.write(b"\0WEST\0").await;
        Timer::after_millis(10).await;

        Timer::after_millis(500).await;

        let _ = leds.write_slice(&[0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00]);

        Timer::after_millis(500).await;
    }
}

#[embassy_executor::task(pool_size = 4)]
async fn rx_task(name: &'static str, mut usart_rx: UsartRx) {
    loop {
        let region = usart_rx.read_until_zero().await;
        if region.first().map(|&f| f == 0).unwrap_or(false) {
            continue;
        }
        //println!(
        //    "{} RX {:?} {:?}",
        //    name,
        //    &region[..],
        //    core::str::from_utf8(&region[..])
        //);
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
        spawner.spawn(main_task(leds, north_tx, south_tx, east_tx, west_tx).unwrap());
        spawner.spawn(rx_task("North", north_rx).expect("north"));
        spawner.spawn(rx_task("South", south_rx).expect("south"));
        spawner.spawn(rx_task("East", east_rx).expect("east"));
        spawner.spawn(rx_task("West", west_rx).expect("west"));
    });
}
