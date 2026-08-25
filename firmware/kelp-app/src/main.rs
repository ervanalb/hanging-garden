#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::{cell::RefCell, ops::DerefMut};

use embassy_futures::join::join_array;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, watch::Watch};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use hal::{Hardware, Leds, UsartRx, UsartTx};
use oorandom::Rand64;
use serde::{Deserialize, Serialize};
use static_cell::StaticCell;

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

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

#[derive(Serialize, Deserialize, Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
struct CommState {}

impl CommState {
    fn is_consistent(&self, _other: &Self) -> bool {
        true
    }

    fn propagate(&self) -> [Self; 4] {
        core::array::from_fn(|_| CommState {})
    }
}

fn try_decode_packet<'a>(s: &'a mut [u8]) -> postcard::Result<CommState> {
    let sz = cobs::decode_in_place(s).map_err(|_| postcard::Error::DeserializeBadEncoding)?;
    postcard::de_flavors::crc::from_bytes_u32(&s[..sz], CRC.digest())
}

fn encode_packet<'a>(data: &'a CommState, s: &'a mut [u8]) -> &'a mut [u8] {
    postcard::serialize_with_flavor(
        &data,
        postcard::ser_flavors::crc::CrcModifier::new(
            postcard::ser_flavors::Cobs::try_new(postcard::ser_flavors::Slice::new(s)).unwrap(),
            CRC.digest(),
        ),
    )
    .unwrap()
}

const TRICKLE_IMIN: Duration = Duration::from_millis(100);
const TRICKLE_IMAX: Duration = Duration::from_millis(10_000);
const TRICKLE_K: usize = 1; // redundancy constant

#[derive(Clone, Debug)]
struct TrickleState {
    comm_state: CommState,
    interval: Duration,
    counter: usize,
    t_expiry: Instant,
    interval_expiry: Instant,
    after_t: bool,
}

impl TrickleState {
    fn new(rng: impl DerefMut<Target = Rand64>) -> Self {
        let mut result = TrickleState {
            comm_state: CommState {},
            interval: TRICKLE_IMIN,
            counter: 0,
            t_expiry: Instant::from_ticks(0), // set by `begin_interval()`
            interval_expiry: Instant::from_ticks(0), // set by `begin_interval()`
            after_t: false,
        };
        result.begin_interval(rng);
        hal::println!("Constructing tricklestate: {:?}", result);
        result
    }

    fn begin_interval(&mut self, mut rng: impl DerefMut<Target = Rand64>) {
        hal::println!(
            "Begin interval called, t range={:?}",
            self.interval.as_ticks() / 2..self.interval.as_ticks()
        );
        let now = Instant::now();
        self.counter = 0;
        self.t_expiry = now
            + Duration::from_ticks(
                rng.rand_range(self.interval.as_ticks() / 2..self.interval.as_ticks()),
            );
        self.interval_expiry = now + self.interval;
        self.after_t = false;
    }

    fn double_interval(&mut self) {
        self.interval = Duration::from_ticks(self.interval.as_ticks() * 2).min(TRICKLE_IMAX);
    }
}

static TRICKLE_STATE: StaticCell<Watch<NoopRawMutex, TrickleState, 2>> = StaticCell::new();
static RNG: StaticCell<Mutex<NoopRawMutex, RefCell<Rand64>>> = StaticCell::new();

#[embassy_executor::task(pool_size = 4)]
async fn rx_task(
    name: &'static str,
    mut usart_rx: UsartRx,
    state: &'static Watch<NoopRawMutex, TrickleState, 2>,
    rng: &'static Mutex<NoopRawMutex, RefCell<Rand64>>,
) {
    let mut rx_buffer = heapless::Vec::<_, 300>::new();
    let mut overrun = false;

    let mut state_receiver = state.anon_receiver();
    let state_sender = state.sender();

    loop {
        let start = rx_buffer.len();
        let region = usart_rx.read(rx_buffer.capacity() - rx_buffer.len()).await;
        rx_buffer.extend_from_slice(&region).unwrap();
        // Look for end of frame (\0 byte)
        for i in start..rx_buffer.len() {
            if rx_buffer[i] == b'\0' {
                // Decode slice if it is non-zero length and not overrun
                if i > 0 && !overrun {
                    // Deserialize rx_buffer[..i] into a CommState variable called packet
                    if let Ok(received_comm_state) = try_decode_packet(&mut rx_buffer[..i]) {
                        hal::println!("RX Packet from {}: {:?}", name, received_comm_state);
                        // We got a valid packet--update the state
                        let mut current_state = state_receiver.try_get().unwrap();
                        let rng_lock = rng.try_lock().unwrap();
                        let rng = rng_lock.borrow_mut();
                        if received_comm_state > current_state.comm_state {
                            // Received message is new
                            current_state.comm_state = received_comm_state;
                            current_state.interval = TRICKLE_IMIN;
                            current_state.begin_interval(rng);
                            state_sender.send(current_state);
                        } else if received_comm_state < current_state.comm_state {
                            if !current_state.comm_state.is_consistent(&received_comm_state) {
                                // Received message is outdated
                                current_state.interval = TRICKLE_IMIN;
                                current_state.begin_interval(rng);
                                state_sender.send(current_state);
                            } else {
                                // Received message is consistent
                            }
                        } else {
                            // Received message is redundant
                            current_state.counter += 1;
                            state_sender.send(current_state);
                        }
                    }
                }
                // Shift buffer contents left & clear overrun flag
                rx_buffer.drain(..=i);
                overrun = false;
            }
        }
        if rx_buffer.is_full() {
            overrun = true;
        }
    }
}

#[embassy_executor::task()]
async fn tx_task(
    state: &'static Watch<NoopRawMutex, TrickleState, 2>,
    rng: &'static Mutex<NoopRawMutex, RefCell<Rand64>>,
    mut usarts_tx: [UsartTx; 4],
) {
    let mut state_receiver = state.receiver().unwrap();
    let state_sender = state.sender();
    let mut tx_buffers: [_; 4] = core::array::from_fn(|_| [0_u8; 301]);
    loop {
        let now = Instant::now();
        let mut current_state = state_receiver.try_get().unwrap();
        hal::println!("TX loop. Current state is {:?}", current_state);

        // Handle state transitions
        if !current_state.after_t {
            if now >= current_state.t_expiry {
                // Handle timer expiry
                current_state.after_t = true;
                let counter = current_state.counter;
                state_sender.send(current_state);

                if counter < TRICKLE_K {
                    // Propagate the state and send it concurrently out of the 4 USART ports
                    let current_state = state_receiver.try_get().unwrap();
                    let propagated = current_state.comm_state.propagate();

                    let lens: [_; 4] = core::array::from_fn(|i| {
                        let comm_state = &propagated[i];
                        let tx_buffer = &mut tx_buffers[i];
                        // We retain an initial '\0' to improve packet start detection
                        let len = encode_packet(comm_state, &mut tx_buffer[1..]).len() + 1;
                        len
                    });

                    // Would be nice to find a cleaner way to do this...
                    let [u0, u1, u2, u3] = &mut usarts_tx;

                    // BROKEN?
                    //join_array([
                    //    u0.write(&tx_buffers[0][..lens[0]]),
                    //    u1.write(&tx_buffers[1][..lens[1]]),
                    //    u2.write(&tx_buffers[2][..lens[2]]),
                    //    u3.write(&tx_buffers[3][..lens[3]]),
                    //])
                    //.await;
                    hal::println!("SEND {:?}", &tx_buffers[0][..lens[0]]);
                } else {
                    hal::println!("no send");
                }
            } else {
                let timeout = current_state.t_expiry - now;
                let _ = state_receiver.changed().with_timeout(timeout).await;
            }
        } else {
            if now >= current_state.interval_expiry {
                current_state.double_interval();
                {
                    let rng_lock = rng.try_lock().unwrap();
                    let rng = rng_lock.borrow_mut();
                    current_state.begin_interval(rng);
                }
                state_sender.send(current_state);
                continue;
            } else {
                let timeout = current_state.interval_expiry - now;
                let _ = state_receiver.changed().with_timeout(timeout).await;
            }
        }
    }
}

#[qingke_rt::entry]
fn main() -> ! {
    let Hardware {
        leds,
        mut led_pwr,
        usarts_tx,
        usarts_rx: [north_rx, south_rx, east_rx, west_rx],
    } = Hardware::init();

    led_pwr.set_pwr(true);

    riscv::asm::delay(1_000_000); // XXX

    // Create executor
    let executor = embassy_executor::Executor::new();
    let executor = unsafe {
        static mut EXECUTOR: core::mem::MaybeUninit<embassy_executor::Executor> =
            core::mem::MaybeUninit::uninit();
        EXECUTOR.write(executor)
    };

    // TODO: Initialize with unique chip identifier
    let rng: &'static Mutex<NoopRawMutex, RefCell<Rand64>> =
        RNG.init(Mutex::new(RefCell::new(Rand64::new(0))));

    let trickle_state = {
        let rng_lock = rng.try_lock().unwrap();
        let rng = rng_lock.borrow_mut();
        TRICKLE_STATE.init(Watch::new_with(TrickleState::new(rng)))
    };

    executor.run(|spawner| {
        spawner.spawn(main_task(leds).unwrap());
        spawner.spawn(rx_task("North", north_rx, trickle_state, rng).unwrap());
        //spawner.spawn(rx_task("South", south_rx, trickle_state, rng).unwrap());
        //spawner.spawn(rx_task("East", east_rx, trickle_state, rng).unwrap());
        //spawner.spawn(rx_task("West", west_rx, trickle_state, rng).unwrap());
        spawner.spawn(tx_task(trickle_state, rng, usarts_tx).unwrap());
    });
}
