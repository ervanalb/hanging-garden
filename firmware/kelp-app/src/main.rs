#![no_std]
#![no_main]

use embassy_futures::join::join_array;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use hal::{Hardware, Leds, UsartRx, UsartTx};
use proto::CommState;
use static_cell::StaticCell;
use trickle::{TrickleParams, TricklePollResult, TrickleState};

static EXECUTOR: StaticCell<embassy_executor::Executor> = StaticCell::new();

static TRICKLE: StaticCell<(
    Mutex<NoopRawMutex, TrickleState<CommState>>,
    Signal<NoopRawMutex, ()>,
)> = StaticCell::new();

#[embassy_executor::task]
async fn main_task(mut leds: Leds) {
    loop {
        //println!("Tick");
        let _ = leds.write_slice(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF]);
        Timer::after_millis(500).await;
        let _ = leds.write_slice(&[0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00]);
        Timer::after_millis(500).await;
    }
}

#[embassy_executor::task(pool_size = 4)]
async fn rx_task(
    name: &'static str,
    mut usart_rx: UsartRx,
    trickle_state: &'static Mutex<NoopRawMutex, TrickleState<'static, CommState>>,
    trickle_signal: &'static Signal<NoopRawMutex, ()>,
) {
    let mut rx_buffer = heapless::Vec::<_, 300>::new();
    let mut overrun = false;

    loop {
        let start = rx_buffer.len();
        let region = usart_rx.read(rx_buffer.capacity() - rx_buffer.len()).await;

        hal::println!("RX from {} bytes: {:?}", name, &region[..]);
        rx_buffer.extend_from_slice(&region).unwrap();
        // Look for end of frame (\0 byte)
        let mut i = start;
        while i < rx_buffer.len() {
            if rx_buffer[i] == b'\0' {
                // Decode slice if it is non-zero length and not overrun
                if i > 0 && !overrun {
                    // Deserialize rx_buffer[..i]
                    if let Ok(received_comm_state) =
                        CommState::try_deserialize_packet(&mut rx_buffer[..i])
                    {
                        hal::println!("RX from {}: packet {:?}", name, received_comm_state);
                        // We got a valid packet--update the state

                        let mut trickle_state = trickle_state
                            .try_lock()
                            .expect("trickle lock should not be held across .awaits");
                        let now = Instant::now();
                        if trickle_state.receive_state(now, &received_comm_state) {
                            trickle_signal.signal(());
                        }
                    }
                }
                // Shift buffer contents left & clear overrun flag
                rx_buffer.drain(..=i);
                overrun = false;
                i = 0;
            } else {
                i += 1;
            }
        }
        if rx_buffer.is_full() {
            overrun = true;
        }
    }
}

#[embassy_executor::task()]
async fn tx_task(
    mut usarts_tx: [UsartTx; 4],
    trickle_state: &'static Mutex<NoopRawMutex, TrickleState<'static, CommState>>,
    trickle_signal: &'static Signal<NoopRawMutex, ()>,
) {
    let mut tx_buffers: [_; 4] = core::array::from_fn(|_| [0_u8; 301]);
    loop {
        let now = Instant::now();

        let mut trickle_state = trickle_state
            .try_lock()
            .expect("trickle lock should not be held across .awaits");
        match trickle_state.poll(now) {
            TricklePollResult::Wait(timeout) => {
                // drop the lock before .await
                drop(trickle_state);

                // Wait for the alotted time,
                // or until we are interrupted from rx_task
                let _ = trickle_signal
                    .wait()
                    .with_timeout(Duration::from_millis(timeout as u64))
                    .await;
            }
            TricklePollResult::Send => {
                let propagated = trickle_state.state().propagate();

                let lens: [_; 4] = core::array::from_fn(|i| {
                    let comm_state = &propagated[i];
                    let tx_buffer = &mut tx_buffers[i];
                    // We retain an initial '\0' to improve packet start detection
                    let len = comm_state.serialize_packet(&mut tx_buffer[1..]).len() + 1;
                    len
                });

                // Would be nice to find a cleaner way to do this...
                let [u0, u1, u2, u3] = &mut usarts_tx;

                // drop the lock before .await
                drop(trickle_state);

                join_array([
                    u0.write(&tx_buffers[0][..lens[0]]),
                    u1.write(&tx_buffers[1][..lens[1]]),
                    u2.write(&tx_buffers[2][..lens[2]]),
                    u3.write(&tx_buffers[3][..lens[3]]),
                ])
                .await;
                hal::println!("TX {:?}", &tx_buffers[0][..lens[0]]);
            }
        }
    }
}

const TRICKLE_PARAMS: TrickleParams = TrickleParams {
    i_min_millis: 100,
    i_max_millis: 10_000,
    k: 1,
};

#[qingke_rt::entry]
fn main() -> ! {
    let Hardware {
        leds,
        mut led_pwr,
        usarts_tx,
        usarts_rx: [north_rx, south_rx, east_rx, west_rx],
    } = Hardware::init();

    led_pwr.set_pwr(true);

    // Create executor
    let executor = EXECUTOR.init(embassy_executor::Executor::new());

    // TODO: Initialize RNG with unique chip identifier
    let now = Instant::now();
    let trickle_state = Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, 0));
    let trickle_signal = Signal::new();
    let (trickle_state, trickle_signal) = TRICKLE.init((trickle_state, trickle_signal));

    executor.run(|spawner| {
        spawner.spawn(main_task(leds).unwrap());
        spawner.spawn(rx_task("North", north_rx, trickle_state, trickle_signal).unwrap());
        spawner.spawn(rx_task("South", south_rx, trickle_state, trickle_signal).unwrap());
        spawner.spawn(rx_task("East", east_rx, trickle_state, trickle_signal).unwrap());
        spawner.spawn(rx_task("West", west_rx, trickle_state, trickle_signal).unwrap());
        spawner.spawn(tx_task(usarts_tx, trickle_state, trickle_signal).unwrap());
    });
}
