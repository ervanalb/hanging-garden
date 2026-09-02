#![no_std]
#![no_main]

use embassy_futures::join::join_array;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use hal::{Flash, Hardware, UsartRx, UsartTx};
use proto::{CommState, CommType, MAX_PACKET_LEN, TRICKLE_PARAMS};
use static_cell::StaticCell;
use trickle::{TrickleOrd, TrickleOrdering, TricklePollResult, TrickleState};

static EXECUTOR: StaticCell<embassy_executor::Executor> = StaticCell::new();

static STATE: StaticCell<(
    Mutex<NoopRawMutex, CommState>,
    Mutex<NoopRawMutex, TrickleState>,
    Signal<NoopRawMutex, ()>,
    Mutex<NoopRawMutex, bool>,
    Mutex<NoopRawMutex, BlState>,
    Mutex<NoopRawMutex, Flash>,
)> = StaticCell::new();

enum BlState {
    Init,
    Ping(u64),
    CodeWrite(u32),
}

#[embassy_executor::task]
async fn main_task(did_receive_packet: &'static Mutex<NoopRawMutex, bool>) {
    Timer::after_millis(500).await;
    // If no bootloader packets were received in the first 500ms,
    // branch to the app.
    {
        let did_receive_packet = did_receive_packet
            .try_lock()
            .expect("did_receive_packet should not be held across .awaits");
        if !*did_receive_packet {
            // Safety: we are not in an interrupt
            unsafe {
                hal::branch_to_app();
            }
        }
    }
    // Wait forever
    core::future::pending::<()>().await;
}

#[embassy_executor::task(pool_size = 4)]
async fn rx_task(
    name: &'static str,
    mut usart_rx: UsartRx,
    comm_state: &'static Mutex<NoopRawMutex, CommState>,
    trickle_state: &'static Mutex<NoopRawMutex, TrickleState<'static>>,
    trickle_signal: &'static Signal<NoopRawMutex, ()>,
    did_receive_packet: &'static Mutex<NoopRawMutex, bool>,
    bl_state: &'static Mutex<NoopRawMutex, BlState>,
    flash: &'static Mutex<NoopRawMutex, Flash>,
) {
    let mut rx_buffer = heapless::Vec::<_, MAX_PACKET_LEN>::new();
    let mut overrun = false;

    loop {
        let start = rx_buffer.len();
        let region = usart_rx.read(rx_buffer.capacity() - rx_buffer.len()).await;

        rx_buffer.extend_from_slice(&region).unwrap();
        // Look for end of frame (\0 byte)
        let mut i = start;
        while i < rx_buffer.len() {
            if rx_buffer[i] == b'\0' {
                // Decode slice if it is non-zero length and not overrun
                if i > 0 && !overrun {
                    // Deserialize rx_buffer[..i]
                    //hal::println!("RX from {} framed bytes: {:?}", name, &rx_buffer[..=i]);
                    if let Ok(received_comm_state) =
                        CommState::try_deserialize_packet(&mut rx_buffer[..=i]).map_err(|e| {
                            //hal::println!("RX err: {:?}", e);
                            e
                        })
                    {
                        let now = Instant::now();
                        //hal::println!("RX {}: packet {:?}", name, received_comm_state);
                        // We got a valid packet--update the state

                        let mut trickle_state = trickle_state
                            .try_lock()
                            .expect("trickle lock should not be held across .awaits");

                        let mut comm_state = comm_state
                            .try_lock()
                            .expect("comm_state lock should not be held across .awaits");
                        match comm_state.consider(&received_comm_state) {
                            TrickleOrdering::Greater => {
                                // Receiving a newer state means that we should assume it.
                                *comm_state = received_comm_state;
                                comm_state.update(now);

                                // Handle special states
                                let mut bl_state = bl_state
                                    .try_lock()
                                    .expect("bl_state lock should not be held across .awaits");

                                let CommState { seq_num, type_ } = &mut *comm_state;
                                match type_ {
                                    CommType::Unknown => {
                                        // Reboot into app
                                        // Safety: we are not in an interrupt
                                        unsafe {
                                            hal::branch_to_app();
                                        }
                                    }
                                    CommType::BlInit => {
                                        *bl_state = BlState::Init;
                                    }
                                    CommType::BlBroadcastPing(bl_broadcast_ping) => {
                                        match *bl_state {
                                            BlState::Ping(sn) if sn == *seq_num => {
                                                // If we've already seen this ping, there is nothing to
                                                // alter about it
                                            }
                                            _ => {
                                                // If this is the first time we've seen this ping,
                                                // mark it with our observed latency.
                                                bl_broadcast_ping.latency_micros =
                                                    bl_broadcast_ping.age_micros.age_micros;
                                                *bl_state = BlState::Ping(comm_state.seq_num)
                                            }
                                        }
                                    }
                                    CommType::BlCodeWrite(bl_code_write) => {
                                        let mut chunk_count = match *bl_state {
                                            BlState::CodeWrite(chunk_count) => chunk_count,
                                            _ => 0,
                                        };
                                        if chunk_count == bl_code_write.chunk_index {
                                            let mut flash = flash.try_lock().expect(
                                                "flash lock should not be held across .awaits",
                                            );
                                            let address =
                                                0x08008000 + 256 * bl_code_write.chunk_index; // TODO use linker symbol
                                            flash.write_page(address, &bl_code_write.chunk_data);
                                            chunk_count += 1;
                                        }
                                        *bl_state = BlState::CodeWrite(chunk_count);
                                    }
                                    CommType::BlCodeProgress(bl_code_progress) => {
                                        let chunk_count = match *bl_state {
                                            BlState::CodeWrite(chunk_count) => chunk_count,
                                            _ => {
                                                *bl_state = BlState::CodeWrite(0);
                                                0
                                            }
                                        };
                                        bl_code_progress.chunk_count = chunk_count;
                                    }
                                    CommType::BlUnknown => {}
                                }
                                {
                                    let mut did_receive_packet =
                                        did_receive_packet.try_lock().expect(
                                            "did_receive_packet should not be held across .awaits",
                                        );
                                    *did_receive_packet = true;
                                }

                                trickle_state.got_new_state(now);
                            }
                            TrickleOrdering::Consistent => {
                                trickle_state.got_outdated_state(now);
                            }
                            TrickleOrdering::Less => {
                                trickle_state.got_outdated_state(now);
                            }
                        }

                        // Wake the event loop
                        trickle_signal.signal(());
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
    comm_state: &'static Mutex<NoopRawMutex, CommState>,
    trickle_state: &'static Mutex<NoopRawMutex, TrickleState<'static>>,
    trickle_signal: &'static Signal<NoopRawMutex, ()>,
) {
    let mut tx_buffers: [_; 4] = core::array::from_fn(|_| [0_u8; MAX_PACKET_LEN + 1]);
    loop {
        let now = Instant::now();

        let mut trickle_state = trickle_state
            .try_lock()
            .expect("trickle lock should not be held across .awaits");
        match trickle_state.poll(now) {
            TricklePollResult::Wait(timeout_micros) => {
                // drop the lock before .await
                drop(trickle_state);

                // Wait for the alotted time,
                // or until we are interrupted from rx_task
                let _ = trickle_signal
                    .wait()
                    .with_timeout(Duration::from_micros(timeout_micros))
                    .await;
            }
            TricklePollResult::Send => {
                let mut comm_state = comm_state
                    .try_lock()
                    .expect("comm_state lock should not be held across .awaits");
                comm_state.update(now);
                let propagated = comm_state.propagate();

                let lens: [_; 4] = core::array::from_fn(|i| {
                    let transmit_comm_state = &propagated[i];
                    //hal::println!(
                    //    "TX {}: {:?}",
                    //    ["North", "South", "East", "West"][i],
                    //    &transmit_comm_state
                    //);
                    let tx_buffer = &mut tx_buffers[i];
                    // We retain an initial '\0' to improve packet start detection
                    let len = transmit_comm_state
                        .serialize_packet(&mut tx_buffer[1..])
                        .len()
                        + 1;
                    len
                });
                //hal::println!("TX west: {:?}", &tx_buffers[3][..lens[3]]);

                // Would be nice to find a cleaner way to do this...
                let [u0, u1, u2, u3] = &mut usarts_tx;

                // drop the locks before .await
                drop(trickle_state);
                drop(comm_state);

                join_array([
                    u0.write(&tx_buffers[0][..lens[0]]),
                    u1.write(&tx_buffers[1][..lens[1]]),
                    u2.write(&tx_buffers[2][..lens[2]]),
                    u3.write(&tx_buffers[3][..lens[3]]),
                ])
                .await;
            }
        }
    }
}

#[qingke_rt::entry]
fn main() -> ! {
    let Hardware {
        leds: _,
        mut led_pwr,
        usarts_tx,
        usarts_rx: [north_rx, south_rx, east_rx, west_rx],
        flash,
    } = Hardware::init(true);

    led_pwr.set_pwr(true);

    // Create executor
    let executor = EXECUTOR.init(embassy_executor::Executor::new());

    // TODO: Initialize RNG with unique chip identifier
    let now = Instant::now();
    let comm_state = Mutex::new(CommState::default());
    let trickle_state = Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, 0));
    let trickle_signal = Signal::new();
    let did_receive_packet = Mutex::new(false);
    let bl_state = Mutex::new(BlState::Init);
    let flash = Mutex::new(flash);
    let (comm_state, trickle_state, trickle_signal, did_receive_packet, bl_state, flash) = STATE
        .init((
            comm_state,
            trickle_state,
            trickle_signal,
            did_receive_packet,
            bl_state,
            flash,
        ));

    executor.run(|spawner| {
        spawner.spawn(main_task(did_receive_packet).unwrap());
        for (name, rx) in [
            ("North", north_rx),
            ("South", south_rx),
            ("East", east_rx),
            ("West", west_rx),
        ] {
            spawner.spawn(
                rx_task(
                    name,
                    rx,
                    comm_state,
                    trickle_state,
                    trickle_signal,
                    did_receive_packet,
                    bl_state,
                    flash,
                )
                .unwrap(),
            );
        }
        spawner.spawn(tx_task(usarts_tx, comm_state, trickle_state, trickle_signal).unwrap());
    });
}
