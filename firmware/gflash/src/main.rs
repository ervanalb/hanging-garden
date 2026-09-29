use log::{debug, error, info, trace};
use proto::{
    BlBroadcastPing, BlCodeProgress, BlCodeWrite, CommState, CommType, MAX_PACKET_LEN, MergeResult,
    TRICKLE_PARAMS,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};
use tokio::time::timeout;
use tokio_serial::SerialPortBuilderExt;
use trickle::{TricklePollResult, TrickleState};

async fn rx_task(
    serial_rx: tokio::io::ReadHalf<tokio_serial::SerialStream>,
    comm_state: &Mutex<CommState>,
    trickle_state: &Mutex<TrickleState<'static>>,
    trickle_notify: &watch::Sender<()>,
    new_comm_notify: &watch::Sender<()>,
) {
    let mut reader = BufReader::new(serial_rx);
    let mut rx_buffer = Vec::new();

    loop {
        rx_buffer.clear();

        match reader.read_until(b'\0', &mut rx_buffer).await {
            Ok(0) => {
                error!("RX error: no data");
                return;
            }
            Ok(1) => {
                // Discard empty packets
            }
            Ok(n) => {
                // Deserialize rx_buffer[..n-1] (excluding the \0)
                trace!("RX bytes: {:?}", &rx_buffer[..n]);
                if let Ok(received_comm_state) =
                    CommState::try_deserialize_packet(&mut rx_buffer[..n]).map_err(|e| {
                        debug!("RX err: {:?}", e);
                        e
                    })
                {
                    let now = Instant::now();
                    debug!("RX: {:?}", received_comm_state);
                    // We got a valid packet--update the state

                    let result = {
                        let mut comm_state = comm_state.lock().await;
                        comm_state.merge(&received_comm_state)
                    };

                    {
                        let mut trickle_state = trickle_state.lock().await;
                        if result == MergeResult::CONSISTENT {
                            trickle_state.got_consistent_state();
                        } else {
                            trickle_state.got_inconsistent_state(now);
                            let _ = trickle_notify.send(());
                        }
                    }

                    if result.newer {
                        let _ = new_comm_notify.send(());
                    }
                }
            }
            Err(e) => {
                error!("RX error: {}", e);
                return;
            }
        }
    }
}

fn new_comm_state(
    new_comm_state: CommState,
    comm_state: &mut CommState,
    trickle_state: &mut TrickleState<'static>,
    trickle_notify: &watch::Sender<()>,
    new_comm_notify: &watch::Sender<()>,
    now: Instant,
) {
    *comm_state = new_comm_state;
    comm_state.update(now);
    trickle_state.got_inconsistent_state(now);
    let _ = trickle_notify.send(());
    let _ = new_comm_notify.send(());
}

async fn tx_task(
    mut serial_tx: tokio::io::WriteHalf<tokio_serial::SerialStream>,
    comm_state: &Mutex<CommState>,
    trickle_state: &Mutex<TrickleState<'static>>,
    mut trickle_notify_rx: watch::Receiver<()>,
) {
    loop {
        let now = Instant::now();

        let trickle_poll = {
            let mut trickle_state = trickle_state.lock().await;
            trickle_state.poll(now)
        };
        match trickle_poll {
            TricklePollResult::Wait(timeout_micros) => {
                // Drop the lock before waiting

                // Wait for the allotted time, or until we are interrupted from rx_task
                tokio::select! {
                    _ = trickle_notify_rx.changed() => {},
                    _ = tokio::time::sleep(Duration::from_micros(timeout_micros)) => {},
                }
            }
            TricklePollResult::Send => {
                let tx_buffer = {
                    let mut comm_state = comm_state.lock().await;
                    comm_state.update(now);
                    debug!("TX: {:?}", &comm_state);
                    let mut tx_buffer = vec![0u8; MAX_PACKET_LEN + 1];
                    // We retain an initial '\0' to improve packet start detection
                    let len = comm_state.serialize_packet(&mut tx_buffer[1..]).len() + 1;
                    tx_buffer.truncate(len);
                    tx_buffer
                };
                trace!("TX bytes: {:?}", &tx_buffer);

                if let Err(e) = serial_tx.write_all(&tx_buffer).await {
                    error!("TX error: {}", e);
                    return;
                }
            }
        }
    }
}

const HIGH_PRIORITY_SEQ_INCREMENT: u64 = 8;
const LONG_TIMEOUT: Duration = Duration::from_millis(1_000);
const INITIAL_CODE_CHUNK_TIMEOUT: Duration = Duration::from_millis(180);
const HARDWARE_ID: u32 = 1;
const CHUNK_SIZE: usize = 256;

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

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
        error!("Error: Firmware file is empty");
        std::process::exit(1);
    }
    let chunk_count = (firmware_data.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;

    // Calculate CRC32 of the entire firmware
    let mut digest = CRC.digest();
    digest.update(&firmware_data);
    let firmware_crc32 = digest.finalize();

    info!(
        "Loaded firmware: {} bytes ({} chunks), CRC32: 0x{:08X}",
        firmware_data.len(),
        chunk_count,
        firmware_crc32
    );

    // Open serial port
    let port = tokio_serial::new(port_name, 115_200).open_native_async()?;

    info!("Opened serial port: {}", port_name);

    // Split the port for RX and TX
    let (serial_rx, serial_tx) = tokio::io::split(port);

    // Initialize trickle state
    let now = Instant::now();
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let comm_state = Mutex::new(CommState::default());
    let trickle_state = Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, seed));
    let (trickle_notify_tx, trickle_notify_rx) = watch::channel(());
    let (new_comm_state_notify_tx, mut new_comm_state_notify_rx) = watch::channel(());

    // Spawn tasks
    let rx_handle = rx_task(
        serial_rx,
        &comm_state,
        &trickle_state,
        &trickle_notify_tx,
        &new_comm_state_notify_tx,
    );
    let tx_handle = tx_task(serial_tx, &comm_state, &trickle_state, trickle_notify_rx);
    let main_handle = async {
        // Switch to bootloader mode
        let mut seq_num = 0;
        info!("Switching to bootloader...");
        let switch_to_bl = async {
            {
                let now = Instant::now();
                let mut comm_state = comm_state
                    .try_lock()
                    .expect("comm_state lock cannot be held across an .await");
                let mut trickle_state = trickle_state
                    .try_lock()
                    .expect("trickle_state lock cannot be held across an .await");
                seq_num = comm_state.seq_num + HIGH_PRIORITY_SEQ_INCREMENT;
                new_comm_state(
                    CommState {
                        seq_num,
                        type_: CommType::BlInit,
                    },
                    &mut *comm_state,
                    &mut *trickle_state,
                    &trickle_notify_tx,
                    &new_comm_state_notify_tx,
                    now,
                );
            }
            loop {
                new_comm_state_notify_rx.changed().await.unwrap(); // TODO real error handling here
                let mut comm_state = comm_state
                    .try_lock()
                    .expect("comm_state lock cannot be held across an .await");
                if comm_state.seq_num > seq_num {
                    // Update with higher seq_num:
                    // One-up the message with an even higher seq_num
                    seq_num = comm_state.seq_num + HIGH_PRIORITY_SEQ_INCREMENT;
                    let mut trickle_state = trickle_state
                        .try_lock()
                        .expect("trickle_state lock cannot be held across an .await");
                    new_comm_state(
                        CommState {
                            seq_num,
                            type_: CommType::BlInit,
                        },
                        &mut *comm_state,
                        &mut *trickle_state,
                        &trickle_notify_tx,
                        &new_comm_state_notify_tx,
                        now,
                    );
                }
            }
        };
        let _ = timeout(LONG_TIMEOUT, switch_to_bl).await;

        info!("Measuring network latency...");
        {
            let now = Instant::now();
            let mut comm_state = comm_state
                .try_lock()
                .expect("comm_state lock cannot be held across an .await");
            let mut trickle_state = trickle_state
                .try_lock()
                .expect("trickle_state lock cannot be held across an .await");
            seq_num = comm_state.seq_num + 1;
            new_comm_state(
                CommState {
                    seq_num,
                    type_: CommType::BlBroadcastPing(BlBroadcastPing {
                        data: heapless::Vec::from_slice(&[0xAA; CHUNK_SIZE]).unwrap(),
                        ..Default::default()
                    }),
                },
                &mut *comm_state,
                &mut *trickle_state,
                &trickle_notify_tx,
                &new_comm_state_notify_tx,
                now,
            );
        }

        tokio::time::sleep(LONG_TIMEOUT).await;

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

        info!(
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
                info!("Write chunk {} / {}", chunk_index, chunk_count);
                {
                    let now = Instant::now();
                    let mut comm_state = comm_state
                        .try_lock()
                        .expect("comm_state lock cannot be held across an .await");
                    let mut trickle_state = trickle_state
                        .try_lock()
                        .expect("trickle_state lock cannot be held across an .await");
                    seq_num = comm_state.seq_num + 1;
                    new_comm_state(
                        CommState {
                            seq_num,
                            type_: CommType::BlCodeWrite(BlCodeWrite {
                                hardware_id: HARDWARE_ID,
                                firmware_size_bytes: firmware_data.len() as u32,
                                firmware_crc32,
                                chunk_index: chunk_index as u32,
                                chunk_data: heapless::Vec::from_slice(chunk_data).unwrap(),
                            }),
                        },
                        &mut *comm_state,
                        &mut *trickle_state,
                        &trickle_notify_tx,
                        &new_comm_state_notify_tx,
                        now,
                    );
                }
                tokio::time::sleep(code_chunk_timeout).await;

                {
                    let comm_state = comm_state
                        .try_lock()
                        .expect("comm_state lock cannot be held across an .await");
                    // Make sure there aren't other messages floating around the network
                    assert_eq!(comm_state.seq_num, seq_num); // TODO better error handling
                }
            }

            info!("Checking progress");
            {
                let now = Instant::now();
                let mut comm_state = comm_state
                    .try_lock()
                    .expect("comm_state lock cannot be held across an .await");
                let mut trickle_state = trickle_state
                    .try_lock()
                    .expect("trickle_state lock cannot be held across an .await");
                seq_num = comm_state.seq_num + 1;
                new_comm_state(
                    CommState {
                        seq_num,
                        type_: CommType::BlCodeProgress(BlCodeProgress {
                            hardware_id: HARDWARE_ID,
                            chunk_count: chunk_count as u32,
                        }),
                    },
                    &mut *comm_state,
                    &mut *trickle_state,
                    &trickle_notify_tx,
                    &new_comm_state_notify_tx,
                    now,
                );
            }
            tokio::time::sleep(LONG_TIMEOUT).await;

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
            info!(
                "- Smallest chunk count on the network is {}",
                chunk_count_progress
            );

            if chunk_count_progress as usize == chunk_count {
                break; // All done
            } else {
                // Need to retransmit some chunks
                first_chunk = chunk_count_progress as usize;
                code_chunk_timeout =
                    Duration::from_secs_f64(code_chunk_timeout.as_secs_f64() * 1.2);
            }
        }

        info!("Switching back to app");
        {
            let now = Instant::now();
            let mut comm_state = comm_state
                .try_lock()
                .expect("comm_state lock cannot be held across an .await");
            let mut trickle_state = trickle_state
                .try_lock()
                .expect("trickle_state lock cannot be held across an .await");
            seq_num = comm_state.seq_num + 1;
            new_comm_state(
                CommState {
                    seq_num,
                    type_: CommType::Init,
                },
                &mut *comm_state,
                &mut *trickle_state,
                &trickle_notify_tx,
                &new_comm_state_notify_tx,
                now,
            );
        }
        tokio::time::sleep(LONG_TIMEOUT).await;
    };

    // Wait for any task to complete (exit when any task returns)
    tokio::select! {
        _ = rx_handle => {},
        _ = tx_handle => {},
        _ = main_handle => {},
    }

    Ok(())
}
