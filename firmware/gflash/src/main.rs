use proto::{CommState, MAX_PACKET_LEN, TRICKLE_PARAMS};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify};
use tokio_serial::SerialPortBuilderExt;
use trickle::{TricklePollResult, TrickleState};

async fn rx_task(
    serial_rx: tokio::io::ReadHalf<tokio_serial::SerialStream>,
    trickle_state: &Mutex<TrickleState<'static, CommState>>,
    trickle_notify: &Notify,
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
                println!("RX bytes: {:?}", &rx_buffer[..n]);
                // Deserialize rx_buffer[..n-1] (excluding the \0)
                if let Ok(received_comm_state) =
                    CommState::try_deserialize_packet(&mut rx_buffer[..n - 1])
                {
                    println!("RX packet: {:?}", received_comm_state);
                    // We got a valid packet--update the state
                    let mut trickle_state = trickle_state
                        .try_lock()
                        .expect("trickle lock cannot be held across an .await");
                    let now = Instant::now();
                    if trickle_state.receive_state(now, &received_comm_state) {
                        trickle_notify.notify_one();
                    }
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
    trickle_state: &Mutex<TrickleState<'static, CommState>>,
    trickle_notify: &Notify,
) {
    loop {
        let now = Instant::now();

        let mut trickle_state = trickle_state
            .try_lock()
            .expect("trickle lock cannot be held across an .await");
        match trickle_state.poll(now) {
            TricklePollResult::Wait(timeout) => {
                // Drop the lock before waiting
                drop(trickle_state);

                // Wait for the allotted time, or until we are interrupted from rx_task
                tokio::select! {
                    _ = trickle_notify.notified() => {},
                    _ = tokio::time::sleep(Duration::from_millis(timeout as u64)) => {},
                }
            }
            TricklePollResult::Send => {
                let comm_state = trickle_state.state();
                let mut tx_buffer = vec![0u8; MAX_PACKET_LEN + 1];
                // We retain an initial '\0' to improve packet start detection
                let len = comm_state.serialize_packet(&mut tx_buffer[1..]).len() + 1;

                // Drop the lock before writing
                drop(trickle_state);

                if let Err(e) = serial_tx.write_all(&tx_buffer[..len]).await {
                    eprintln!("TX error: {}", e);
                    return;
                } else {
                    println!("TX {:?}", &tx_buffer[..len]);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <serial_port>", args[0]);
        eprintln!("Example: {} /dev/ttyUSB0", args[0]);
        std::process::exit(1);
    }

    let port_name = &args[1];

    // Open serial port
    let port = tokio_serial::new(port_name, 115_200).open_native_async()?;

    println!("Opened serial port: {}", port_name);

    // Split the port for RX and TX
    let (serial_rx, serial_tx) = tokio::io::split(port);

    // Initialize trickle state
    let now = Instant::now();
    let trickle_state = Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, 0));
    let trickle_notify = Notify::new();

    // Spawn tasks
    let rx_handle = rx_task(serial_rx, &trickle_state, &trickle_notify);
    let tx_handle = tx_task(serial_tx, &trickle_state, &trickle_notify);
    let main_handle = async {
        println!("Waiting 100ms to synchronize with network");
        tokio::time::sleep(Duration::from_millis(100)).await;

        {
            let trickle_state = trickle_state.try_lock()
            .expect("trickle lock cannot be held across an .await");
            println!("Done! comm state is {:?}", trickle_state.state());
        }
    };

    // Wait for any task to complete (exit when any task returns)
    tokio::select! {
        _ = rx_handle => {},
        _ = tx_handle => {},
        _ = main_handle => {},
    }

    Ok(())
}
