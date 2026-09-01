use proto::{
    BlBroadcastPing, BlCodeProgress, BlCodeWrite, CommState, CommType, MAX_PACKET_LEN,
    TRICKLE_PARAMS,
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};
use tokio::time::timeout;
use tokio_serial::SerialPortBuilderExt;
use trickle::{TrickleOrd, TrickleOrdering, TricklePollResult, TrickleState};

async fn rx_task(
    serial_rx: tokio::io::ReadHalf<tokio_serial::SerialStream>,
    comm_state: &Mutex<CommState>,
    trickle_state: &Mutex<TrickleState<'static>>,
    trickle_notify: &watch::Sender<()>,
    new_comm_state_tx: watch::Sender<CommState>,
) {
    let mut reader = BufReader::new(serial_rx);
    let mut rx_buffer = Vec::new();

    loop {
        rx_buffer.clear();

        match reader.read_until(b'\0', &mut rx_buffer).await {
            Ok(0) => {
                eprintln!("RX error: no data");
                return;
            }
            Ok(1) => {
                // Discard empty packets
            }
            Ok(n) => {
                // Deserialize rx_buffer[..n-1] (excluding the \0)
                //println!("RX bytes: {:?}", &rx_buffer[..n]);
                if let Ok(received_comm_state) =
                    CommState::try_deserialize_packet(&mut rx_buffer[..n]).map_err(|e| {
                        //println!("RX err: {:?}", e);
                        e
                    })
                {
                    let now = Instant::now();
                    //println!("RX: {:?}", received_comm_state);
                    // We got a valid packet--update the state

                    let mut trickle_state = trickle_state
                        .try_lock()
                        .expect("trickle lock should not be held across .awaits");

                    let comm_state = comm_state
                        .try_lock()
                        .expect("comm_state lock should not be held across .awaits");
                    match comm_state.consider(&received_comm_state) {
                        TrickleOrdering::Greater => {
                            // Receiving a newer state generally means that we should assume it.
                            // But sometimes we want to alter or react to it.
                            // So instead, we send it out, and the other end of the new_comm_state channel
                            // will call *comm_state=received_comm_state, trickle_state.got_new_state() and trickle_notify.send()
                            let _ = new_comm_state_tx.send(received_comm_state);
                        }
                        TrickleOrdering::Consistent => {
                            trickle_state.got_consistent_state();
                            // No need to wake
                        }
                        TrickleOrdering::Less => {
                            trickle_state.got_outdated_state(now);
                            let _ = trickle_notify.send(());
                        }
                    }

                    // Wake the event loop
                    let _ = trickle_notify.send(());
                }
            }
            Err(e) => {
                eprintln!("RX error: {}", e);
                return;
            }
        }
    }
}

async fn tx_task(
    mut serial_tx: tokio::io::WriteHalf<tokio_serial::SerialStream>,
    comm_state: &Mutex<CommState>,
    trickle_state: &Mutex<TrickleState<'static>>,
    mut trickle_notify_rx: watch::Receiver<()>,
) {
    loop {
        let now = Instant::now();

        let mut trickle_state = trickle_state
            .try_lock()
            .expect("trickle lock cannot be held across an .await");
        match trickle_state.poll(now) {
            TricklePollResult::Wait(timeout_micros) => {
                // Drop the lock before waiting
                drop(trickle_state);

                // Wait for the allotted time, or until we are interrupted from rx_task
                tokio::select! {
                    _ = trickle_notify_rx.changed() => {},
                    _ = tokio::time::sleep(Duration::from_micros(timeout_micros)) => {},
                }
            }
            TricklePollResult::Send => {
                let mut comm_state = comm_state
                    .try_lock()
                    .expect("comm_state lock cannot be held across an .await");
                comm_state.update(now);
                //println!("TX {:?}", &comm_state);
                let mut tx_buffer = vec![0u8; MAX_PACKET_LEN + 1];
                // We retain an initial '\0' to improve packet start detection
                let len = comm_state.serialize_packet(&mut tx_buffer[1..]).len() + 1;
                //println!("TX bytes: {:?}", &tx_buffer[..len]);

                // Drop the locks before writing
                drop(comm_state);
                drop(trickle_state);

                if let Err(e) = serial_tx.write_all(&tx_buffer[..len]).await {
                    eprintln!("TX error: {}", e);
                    return;
                }
            }
        }
    }
}

const HIGH_PRIORITY_SEQ_INCREMENT: u64 = 8;
const LONG_TIMEOUT: Duration = Duration::from_millis(1_000);
const INITIAL_CODE_CHUNK_TIMEOUT: Duration = Duration::from_millis(50);
const HARDWARE_ID: u32 = 1;
const CHUNK_SIZE: usize = 256;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <serial_port> <firmware_binary>", args[0]);
        eprintln!("Example: {} /dev/ttyUSB0 firmware.bin", args[0]);
        std::process::exit(1);
    }

    let port_name = &args[1];
    let firmware_path = &args[2];

    // Read firmware binary
    let firmware_data = std::fs::read(firmware_path)?;
    if firmware_data.is_empty() {
        eprintln!("Error: Firmware file is empty");
        std::process::exit(1);
    }
    let chunk_count = (firmware_data.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;
    println!(
        "Loaded firmware: {} bytes ({} chunks)",
        firmware_data.len(),
        chunk_count
    );

    // Open serial port
    let port = tokio_serial::new(port_name, 115_200).open_native_async()?;

    println!("Opened serial port: {}", port_name);

    // Split the port for RX and TX
    let (serial_rx, serial_tx) = tokio::io::split(port);

    // Initialize trickle state
    let now = Instant::now();
    let comm_state = Mutex::new(CommState::default());
    let trickle_state = Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, 42)); // TODO: Seed
    let (trickle_notify_tx, trickle_notify_rx) = watch::channel(());
    let (new_comm_state_tx, mut new_comm_state_rx) = watch::channel(CommState::default());

    // Spawn tasks
    let rx_handle = rx_task(
        serial_rx,
        &comm_state,
        &trickle_state,
        &trickle_notify_tx,
        new_comm_state_tx,
    );
    let tx_handle = tx_task(serial_tx, &comm_state, &trickle_state, trickle_notify_rx);
    let main_handle = async {
        // Switch to bootloader mode
        let mut seq_num = 0;
        println!("Switching to bootloader...");
        let switch_to_bl = async {
            loop {
                {
                    let now = Instant::now();
                    let mut comm_state = comm_state
                        .try_lock()
                        .expect("comm_state lock cannot be held across an .await");
                    let mut trickle_state = trickle_state
                        .try_lock()
                        .expect("trickle_state lock cannot be held across an .await");
                    seq_num = comm_state.seq_num + HIGH_PRIORITY_SEQ_INCREMENT;
                    *comm_state = CommState {
                        seq_num,
                        type_: CommType::BlInit,
                    };
                    comm_state.update(now);
                    trickle_state.got_new_state(now);
                    let _ = trickle_notify_tx.send(());
                }
                loop {
                    new_comm_state_rx.changed().await.unwrap(); // TODO real error handling here
                    let new_comm_state = new_comm_state_rx.borrow_and_update();
                    if new_comm_state.seq_num > seq_num {
                        break; // Send the message again with higher seq num
                    } else {
                        // Basic update
                        let mut comm_state = comm_state
                            .try_lock()
                            .expect("comm_state lock cannot be held across an .await");
                        let mut trickle_state = trickle_state
                            .try_lock()
                            .expect("trickle_state lock cannot be held across an .await");
                        *comm_state = new_comm_state.clone();
                        trickle_state.got_new_state(now);
                        let _ = trickle_notify_tx.send(());
                    }
                }
            }
        };
        let _ = timeout(LONG_TIMEOUT, switch_to_bl).await;

        println!("Measuring network latency...");
        let broadcast_ping = async {
            {
                let now = Instant::now();
                let mut comm_state = comm_state
                    .try_lock()
                    .expect("comm_state lock cannot be held across an .await");
                let mut trickle_state = trickle_state
                    .try_lock()
                    .expect("trickle_state lock cannot be held across an .await");
                seq_num = comm_state.seq_num + 1;
                *comm_state = CommState {
                    seq_num,
                    type_: CommType::BlBroadcastPing(BlBroadcastPing {
                        data: heapless::Vec::from_slice(&[0xAA; CHUNK_SIZE]).unwrap(),
                        ..Default::default()
                    }),
                };
                comm_state.update(now);
                trickle_state.got_new_state(now);
                let _ = trickle_notify_tx.send(());
            }

            loop {
                new_comm_state_rx.changed().await.unwrap(); // TODO real error handling here
                let new_comm_state = new_comm_state_rx.borrow_and_update();
                {
                    // Basic update
                    let mut comm_state = comm_state
                        .try_lock()
                        .expect("comm_state lock cannot be held across an .await");
                    let mut trickle_state = trickle_state
                        .try_lock()
                        .expect("trickle_state lock cannot be held across an .await");
                    *comm_state = new_comm_state.clone();
                    trickle_state.got_new_state(now);
                    let _ = trickle_notify_tx.send(());
                }
            }
        };
        let _ = timeout(LONG_TIMEOUT, broadcast_ping).await;

        let latency_micros = {
            let comm_state = comm_state
                .try_lock()
                .expect("comm_state lock cannot be held across an .await");
            // Make sure there aren't other messages floating around the network
            assert_eq!(comm_state.seq_num, seq_num); // TODO better error handling
            match comm_state.type_ {
                CommType::BlBroadcastPing(BlBroadcastPing { latency_micros, .. }) => latency_micros,
                _ => panic!("msg type changed unexpectedly on network"), // TODO better error handling
            }
        };

        if latency_micros == 0 {
            panic!("No devices detected on network"); // TODO better error handling
        }

        println!(
            " - Measured latency: {} milliseconds",
            latency_micros / 1_000
        );
        let mut code_chunk_timeout = INITIAL_CODE_CHUNK_TIMEOUT;
        let mut first_chunk = 0;

        loop {
            // Split firmware into 256-byte chunks
            let firmware_chunks = firmware_data[first_chunk * CHUNK_SIZE..].chunks(CHUNK_SIZE);
            for (chunk_index, chunk_data) in firmware_chunks.enumerate() {
                let chunk_index = chunk_index + first_chunk;
                println!("Write chunk {} / {}", chunk_index, chunk_count);
                let write_chunk = async {
                    {
                        let now = Instant::now();
                        let mut comm_state = comm_state
                            .try_lock()
                            .expect("comm_state lock cannot be held across an .await");
                        let mut trickle_state = trickle_state
                            .try_lock()
                            .expect("trickle_state lock cannot be held across an .await");
                        seq_num = comm_state.seq_num + 1;
                        *comm_state = CommState {
                            seq_num,
                            type_: CommType::BlCodeWrite(BlCodeWrite {
                                hardware_id: HARDWARE_ID,
                                chunk_count: chunk_count as u32,
                                chunk_index: chunk_index as u32,
                                chunk_data: heapless::Vec::from_slice(chunk_data).unwrap(),
                            }),
                        };
                        comm_state.update(now);
                        trickle_state.got_new_state(now);
                        let _ = trickle_notify_tx.send(());
                    }

                    loop {
                        new_comm_state_rx.changed().await.unwrap(); // TODO real error handling here
                        let new_comm_state = new_comm_state_rx.borrow_and_update();
                        {
                            // Basic update
                            let mut comm_state = comm_state
                                .try_lock()
                                .expect("comm_state lock cannot be held across an .await");
                            let mut trickle_state = trickle_state
                                .try_lock()
                                .expect("trickle_state lock cannot be held across an .await");
                            *comm_state = new_comm_state.clone();
                            trickle_state.got_new_state(now);
                            let _ = trickle_notify_tx.send(());
                        }
                    }
                };
                let _ = timeout(code_chunk_timeout, write_chunk).await;

                {
                    let comm_state = comm_state
                        .try_lock()
                        .expect("comm_state lock cannot be held across an .await");
                    // Make sure there aren't other messages floating around the network
                    assert_eq!(comm_state.seq_num, seq_num); // TODO better error handling
                }
            }

            println!("Checking progress");
            let progress = async {
                {
                    let now = Instant::now();
                    let mut comm_state = comm_state
                        .try_lock()
                        .expect("comm_state lock cannot be held across an .await");
                    let mut trickle_state = trickle_state
                        .try_lock()
                        .expect("trickle_state lock cannot be held across an .await");
                    seq_num = comm_state.seq_num + 1;
                    *comm_state = CommState {
                        seq_num,
                        type_: CommType::BlCodeProgress(BlCodeProgress {
                            hardware_id: HARDWARE_ID,
                            chunk_count: chunk_count as u32,
                        }),
                    };
                    comm_state.update(now);
                    trickle_state.got_new_state(now);
                    let _ = trickle_notify_tx.send(());
                }

                loop {
                    new_comm_state_rx.changed().await.unwrap(); // TODO real error handling here
                    let new_comm_state = new_comm_state_rx.borrow_and_update();
                    {
                        // Basic update
                        let mut comm_state = comm_state
                            .try_lock()
                            .expect("comm_state lock cannot be held across an .await");
                        let mut trickle_state = trickle_state
                            .try_lock()
                            .expect("trickle_state lock cannot be held across an .await");
                        *comm_state = new_comm_state.clone();
                        trickle_state.got_new_state(now);
                        let _ = trickle_notify_tx.send(());
                    }
                }
            };
            let _ = timeout(LONG_TIMEOUT, progress).await;

            let chunk_count_progress = {
                let comm_state = comm_state
                    .try_lock()
                    .expect("comm_state lock cannot be held across an .await");
                // Make sure there aren't other messages floating around the network
                assert_eq!(comm_state.seq_num, seq_num); // TODO better error handling
                match comm_state.type_ {
                    CommType::BlCodeProgress(BlCodeProgress {
                        hardware_id: _,
                        chunk_count,
                    }) => chunk_count,
                    _ => panic!("msg type changed unexpectedly on network"), // TODO better error handling
                }
            };
            println!(
                "- Smallest chunk count on the network is {}",
                chunk_count_progress
            );

            if chunk_count_progress as usize == chunk_count {
                break; // All done
            } else {
                // Need to retransmit some chunks
                first_chunk = chunk_count_progress as usize;
                code_chunk_timeout *= 2;
            }
        }

        println!("Switching back to app");
        let switch_to_app = async {
            {
                let now = Instant::now();
                let mut comm_state = comm_state
                    .try_lock()
                    .expect("comm_state lock cannot be held across an .await");
                let mut trickle_state = trickle_state
                    .try_lock()
                    .expect("trickle_state lock cannot be held across an .await");
                seq_num = comm_state.seq_num + 1;
                *comm_state = CommState {
                    seq_num,
                    type_: CommType::Init,
                };
                comm_state.update(now);
                trickle_state.got_new_state(now);
                let _ = trickle_notify_tx.send(());
            }

            loop {
                new_comm_state_rx.changed().await.unwrap(); // TODO real error handling here
                let new_comm_state = new_comm_state_rx.borrow_and_update();
                {
                    // Basic update
                    let mut comm_state = comm_state
                        .try_lock()
                        .expect("comm_state lock cannot be held across an .await");
                    let mut trickle_state = trickle_state
                        .try_lock()
                        .expect("trickle_state lock cannot be held across an .await");
                    *comm_state = new_comm_state.clone();
                    trickle_state.got_new_state(now);
                    let _ = trickle_notify_tx.send(());
                }
            }
        };
        let _ = timeout(LONG_TIMEOUT, switch_to_app).await;
    };

    // Wait for any task to complete (exit when any task returns)
    tokio::select! {
        _ = rx_handle => {},
        _ = tx_handle => {},
        _ = main_handle => {},
    }

    Ok(())
}
