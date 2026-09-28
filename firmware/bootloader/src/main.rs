#![no_std]
#![no_main]

// Compile-time flag to control branching to app
macro_rules! bl_branch_enabled {
    () => {
        !matches!(option_env!("BL_BRANCH"), Some("NO") | Some("no"))
    };
}

/// Bootloader configuration structure placed in BOOTLOADER_CONFIG_USR section
#[repr(C)]
struct BootloaderConfig {
    magic: u32,
    firmware_size_bytes: u32,
    firmware_crc32: u32,
}

/// Static bootloader configuration placed in the BOOTLOADER_CONFIG_USR linker section
#[unsafe(link_section = ".bootloader_config_usr")]
#[used]
static BOOTLOADER_CONFIG: BootloaderConfig = BootloaderConfig {
    magic: 0,
    firmware_size_bytes: 0,
    firmware_crc32: 0,
};

#[allow(improper_ctypes)]
unsafe extern "C" {
    static _sapp_usr: ();
    static _eapp_usr: ();
}

use embassy_futures::join::join_array;
use embassy_sync::{
    blocking_mutex::raw::NoopRawMutex, channel::Channel, mutex::Mutex, signal::Signal,
};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use hal::{Flash, Hardware, UsartRx, UsartTx, Watchdog};
use proto::{CommState, CommType, MAX_PACKET_LEN, TRICKLE_PARAMS};
use static_cell::StaticCell;
use trickle::{TrickleOrd, TrickleOrdering, TricklePollResult, TrickleState};

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

static EXECUTOR: StaticCell<embassy_executor::Executor> = StaticCell::new();

const BOOTLOADER_CONFIG_MAGIC: u32 = 0x7ac37303;

/// Validates the application firmware using the CRC32 from BOOTLOADER_CONFIG and branches to app if valid.
///
/// # Safety
/// This function may branch to application code.
/// It should only be called when not in an interrupt.
async unsafe fn validate_and_branch_to_app(
    bl_state: &mut BlState,
    flash: &Mutex<NoopRawMutex, Flash>,
) {
    let _flash_guard = flash.lock().await;
    // Read the config from flash
    // Safety: BOOTLOADER_CONFIG is at a fixed, valid memory location
    let config_ptr = &BOOTLOADER_CONFIG as *const BootloaderConfig;
    let config = unsafe { core::ptr::read_volatile(config_ptr) };

    // Check magic number
    if config.magic != BOOTLOADER_CONFIG_MAGIC {
        defmt::warn!("Invalid magic number in config: 0x{:08X}", config.magic);
        *bl_state = BlState::InvalidApp;
        return;
    }

    // Check firmware size is reasonable (not zero, not larger than APP flash)
    let app_flash_size =
        unsafe { (&_eapp_usr as *const ()) as u32 - (&_sapp_usr as *const ()) as u32 };
    if config.firmware_size_bytes == 0 || config.firmware_size_bytes > app_flash_size {
        defmt::warn!(
            "Invalid firmware size: {} bytes",
            config.firmware_size_bytes
        );
        *bl_state = BlState::InvalidApp;
        return;
    }

    defmt::info!(
        "Validating app: size={} bytes, expected CRC32=0x{:08X}",
        config.firmware_size_bytes,
        config.firmware_crc32
    );

    // Read application flash and compute CRC32
    let app_flash_start = unsafe { (&_sapp_usr as *const ()) as u32 };
    let mut digest = CRC.digest();

    // Process in chunks to avoid long operations
    let mut offset = 0;
    while offset < config.firmware_size_bytes {
        let chunk_size = (config.firmware_size_bytes - offset).min(256);
        // Safety: Reading from flash memory within the application region
        let chunk_ptr = unsafe { (app_flash_start as *const u8).add(offset as usize) };
        let chunk_slice = unsafe { core::slice::from_raw_parts(chunk_ptr, chunk_size as usize) };
        digest.update(chunk_slice);
        offset += chunk_size;
    }

    let calculated_crc = digest.finalize();

    if calculated_crc == config.firmware_crc32 {
        defmt::info!("App validation successful!");
        // Safety: Caller ensures we're not in an interrupt
        if bl_branch_enabled!() {
            defmt::info!("Branching to app...");
            unsafe {
                hal::branch_to_app();
            }
        } else {
            defmt::info!("Not branching to app (disabled by compile-time flag)");
        }
    } else {
        defmt::warn!(
            "App validation FAILED! Calculated CRC32=0x{:08X}, expected 0x{:08X}",
            calculated_crc,
            config.firmware_crc32
        );
        *bl_state = BlState::InvalidApp;
    }
}

static STATE: StaticCell<(
    Mutex<NoopRawMutex, CommState>,
    Mutex<NoopRawMutex, TrickleState>,
    Signal<NoopRawMutex, ()>,
    Mutex<NoopRawMutex, bool>,
    Mutex<NoopRawMutex, BlState>,
    Mutex<NoopRawMutex, Flash>,
    Channel<NoopRawMutex, proto::BlCodeWrite, 1>,
)> = StaticCell::new();

enum BlState {
    Init,
    Ping(u64),
    CodeWrite {
        chunk_index: u32,
        firmware_size_bytes: u32,
        firmware_crc32: u32,
        stalled: bool,
    },
    InvalidApp,
}

#[embassy_executor::task]
async fn branch_task(
    did_receive_packet: &'static Mutex<NoopRawMutex, bool>,
    bl_state: &'static Mutex<NoopRawMutex, BlState>,
    flash: &'static Mutex<NoopRawMutex, Flash>,
) {
    Timer::after_millis(500).await;
    // If no bootloader packets were received in the first 500ms,
    // validate and branch to the app.
    {
        let did_receive_packet = did_receive_packet
            .try_lock()
            .expect("did_receive_packet should not be held across .awaits");
        if !*did_receive_packet {
            // Safety: we are not in an interrupt
            let mut bl_state_guard = bl_state
                .try_lock()
                .expect("bl_state lock should not be held across .awaits");
            unsafe {
                validate_and_branch_to_app(&mut *bl_state_guard, flash).await;
            }
        }
    }
    // Wait forever
    core::future::pending::<()>().await;
}

#[embassy_executor::task]
async fn led_pwr_task(mut led_pwr: hal::LedPwr) {
    loop {
        // Set LED power on
        led_pwr.set_pwr(true);
        Timer::after_millis(100).await; // Weather the initial spike

        loop {
            // Read current at 100Hz (every 10ms)
            let current = led_pwr.read_current().await;

            if current > 50 {
                defmt::info!("Overcurrent detected: {}, shutting off LEDs", current);
                break;
            }
            Timer::after_millis(10).await;
        }

        // Shut off LED power
        led_pwr.set_pwr(false);

        // Wait 1 second
        Timer::after_secs(1).await;

        defmt::info!("LEDs powered back on");
    }
}

#[embassy_executor::task]
async fn led_task(
    mut leds: hal::Leds,
    bl_state: &'static Mutex<NoopRawMutex, BlState>,
    watchdog: Watchdog,
) {
    Timer::after_millis(100).await; // Avoid startup transients in state that may cause flashing the LEDs
    loop {
        // Read bl_state and generate LED pattern
        let mut led_pattern = [0u8; 30]; // 10 LEDs * 3 bytes (GRB)

        {
            let bl_state = bl_state
                .try_lock()
                .expect("bl_state lock should not be held across .awaits");

            match *bl_state {
                BlState::CodeWrite {
                    chunk_index,
                    firmware_size_bytes,
                    stalled,
                    ..
                } => {
                    // Calculate progress percentage and number of LEDs to light up
                    let bytes_written = chunk_index * 256;
                    let progress_leds = if firmware_size_bytes > 0 {
                        ((bytes_written * 10) / firmware_size_bytes).min(10) as usize
                    } else {
                        0
                    };

                    // Colors in GRB format:
                    // Bright blue (0x0000FF in RGB) = [G=0x00, R=0x00, B=0xFF]
                    // Yellow (0xFFFF00 in RGB) = [G=0xFF, R=0xFF, B=0x00]
                    // Dark blue (0x000001 in RGB) = [G=0x00, R=0x00, B=0x10]
                    let progress_color;
                    let bg_color;
                    if stalled {
                        progress_color = [0xFF, 0xFF, 0x00]; // Yellow (GRB)
                        bg_color = [0x10, 0x10, 0x00]; // Dark Yellow (GRB)
                    } else {
                        progress_color = [0x00, 0x00, 0xFF]; // Blue (GRB)
                        bg_color = [0x00, 0x00, 0x10]; // Dark blue (GRB)
                    };

                    // Fill progress LEDs
                    for i in 0..progress_leds {
                        led_pattern[i * 3] = progress_color[0];
                        led_pattern[i * 3 + 1] = progress_color[1];
                        led_pattern[i * 3 + 2] = progress_color[2];
                    }

                    // Fill remaining LEDs with dark blue
                    for i in progress_leds..10 {
                        led_pattern[i * 3] = bg_color[0];
                        led_pattern[i * 3 + 1] = bg_color[1];
                        led_pattern[i * 3 + 2] = bg_color[2];
                    }
                }
                BlState::InvalidApp => {
                    // Invalid app state, display all dark red
                    for i in 0..10 {
                        led_pattern[i * 3] = 0x00; // G
                        led_pattern[i * 3 + 1] = 0x10; // R
                        led_pattern[i * 3 + 2] = 0x00; // B
                    }
                }
                _ => {
                    // Not in CodeWrite state, display all dark blue
                    for i in 0..10 {
                        led_pattern[i * 3] = 0x00; // G
                        led_pattern[i * 3 + 1] = 0x00; // R
                        led_pattern[i * 3 + 2] = 0x10; // B
                    }
                }
            }
        }

        // Write LED pattern
        let _ = leds.write_slice(&led_pattern);

        // Feed watchdog
        watchdog.feed();

        // Wait until next frame time
        Timer::after_millis(33).await;
    }
}

#[embassy_executor::task]
async fn flash_writer_task(
    flash_channel: &'static Channel<NoopRawMutex, proto::BlCodeWrite, 1>,
    flash: &'static Mutex<NoopRawMutex, Flash>,
    bl_state: &'static Mutex<NoopRawMutex, BlState>,
) {
    let app_flash_start = unsafe { (&_sapp_usr as *const ()) as u32 };

    loop {
        let msg = flash_channel.receive().await;

        defmt::debug!("Receive chunk");

        let (mut chunk_index, crc_changed) = {
            let bl_state = bl_state
                .try_lock()
                .expect("bl_state lock should not be held across .awaits");
            match &*bl_state {
                BlState::CodeWrite {
                    chunk_index,
                    firmware_crc32,
                    ..
                } if *firmware_crc32 == msg.firmware_crc32 => (*chunk_index, false),
                _ => (0, true),
            }
        };

        // If CRC changed (indicating new firmware), write the bootloader config flash page
        if crc_changed {
            defmt::info!(
                "Writing bootloader config for new firmware: Size: {} bytes, CRC32: 0x{:08X}",
                msg.firmware_size_bytes,
                msg.firmware_crc32
            );

            let config = BootloaderConfig {
                magic: BOOTLOADER_CONFIG_MAGIC,
                firmware_size_bytes: msg.firmware_size_bytes,
                firmware_crc32: msg.firmware_crc32,
            };

            let config_bytes = unsafe {
                core::slice::from_raw_parts(
                    &config as *const BootloaderConfig as *const u8,
                    core::mem::size_of::<BootloaderConfig>(),
                )
            };

            let mut flash = flash.lock().await;
            let config_address = &BOOTLOADER_CONFIG as *const BootloaderConfig as u32;
            flash.write_page(config_address, config_bytes).await;
            defmt::info!("Bootloader config written to flash");
        }

        // Calculate chunk count from firmware size
        let chunk_count = (msg.firmware_size_bytes + 255) / 256;

        let stalled;

        // Validate chunk index is not beyond the last expected chunk
        if msg.chunk_index >= chunk_count {
            stalled = true;
            defmt::warn!(
                "Ignoring chunk {} beyond chunk_count {} (firmware size: {} bytes)",
                msg.chunk_index,
                chunk_count,
                msg.firmware_size_bytes
            );
        } else {
            defmt::info!(
                "Flash write msg, chunk_index={:?} (ours is {:?})",
                msg.chunk_index, chunk_index
            );
            if msg.chunk_index == chunk_index {
                let is_last_chunk = msg.chunk_index == chunk_count - 1;
                let expected_chunk_size = if is_last_chunk {
                    let remainder = msg.firmware_size_bytes % 256;
                    if remainder == 0 {
                        256
                    } else {
                        remainder as usize
                    }
                } else {
                    256
                };

                // Validate chunk size
                if msg.chunk_data.len() != expected_chunk_size {
                    stalled = true;
                    defmt::warn!(
                        "Chunk {} has size {} bytes, expected {} bytes",
                        msg.chunk_index,
                        msg.chunk_data.len(),
                        expected_chunk_size
                    );
                } else {
                    let mut flash = flash.lock().await;
                    let address = app_flash_start + 256 * msg.chunk_index;
                    flash.write_page(address, &msg.chunk_data).await;

                    chunk_index = msg.chunk_index + 1;
                    stalled = false;
                    defmt::info!("Write OK!");
                }
            } else if msg.chunk_index > chunk_index {
                stalled = true;
                defmt::info!("We have fallen behind");
            } else {
                stalled = false;
                defmt::info!("We are ahead");
            }
        }
        let mut bl_state = bl_state
            .try_lock()
            .expect("bl_state lock should not be held across .awaits");
        *bl_state = BlState::CodeWrite {
            chunk_index,
            firmware_size_bytes: msg.firmware_size_bytes,
            firmware_crc32: msg.firmware_crc32,
            stalled,
        };
    }
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
    flash_channel: &'static Channel<NoopRawMutex, proto::BlCodeWrite, 1>,
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
                    defmt::trace!("RX from {} framed bytes: {:?}", name, &rx_buffer[..=i]);
                    if let Ok(received_comm_state) =
                        CommState::try_deserialize_packet(&mut rx_buffer[..=i]).map_err(|e| {
                            defmt::debug!("RX err: {:?}", defmt::Debug2Format(&e));
                            e
                        })
                    {
                        let now = Instant::now();
                        defmt::debug!("RX {}: packet {:?}", name, received_comm_state);
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
                                        // Validate and reboot into app
                                        // Safety: we are not in an interrupt
                                        unsafe {
                                            validate_and_branch_to_app(&mut *bl_state, flash).await;
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
                                        // Send the flash write message to the flash writer task
                                        let _ = flash_channel.try_send(bl_code_write.clone());
                                    }
                                    CommType::BlCodeProgress(bl_code_progress) => {
                                        let chunk_index = match *bl_state {
                                            BlState::CodeWrite { chunk_index, .. } => chunk_index,
                                            _ => 0,
                                        };
                                        bl_code_progress.chunk_count =
                                            bl_code_progress.chunk_count.min(chunk_index);
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
                                trickle_state.got_consistent_state();
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
            rx_buffer.clear();
            defmt::info!("{} Overrun", name);
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
                    defmt::debug!(
                        "TX {}: {:?}",
                        ["North", "South", "East", "West"][i],
                        &transmit_comm_state
                    );
                    let tx_buffer = &mut tx_buffers[i];
                    // We retain an initial '\0' to improve packet start detection
                    let len = transmit_comm_state
                        .serialize_packet(&mut tx_buffer[1..])
                        .len()
                        + 1;
                    len
                });

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
        leds,
        mut led_pwr,
        usarts_tx,
        usarts_rx: [north_rx, south_rx, east_rx, west_rx],
        flash,
        watchdog,
        chip_id,
    } = Hardware::init();

    defmt::info!("Bootloader started");

    // Create executor
    let executor = EXECUTOR.init(embassy_executor::Executor::new());

    // Seed RNG with unique chip identifier
    let seed = (((chip_id[0] as u64) << 32) | (chip_id[1] as u64)) ^ (chip_id[2] as u64);
    let now = Instant::now();
    let comm_state = Mutex::new(CommState::default());
    let trickle_state = Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, seed));
    let trickle_signal = Signal::new();
    let did_receive_packet = Mutex::new(false);
    let bl_state = Mutex::new(BlState::Init);
    let flash = Mutex::new(flash);
    let flash_channel = Channel::new();
    let (
        comm_state,
        trickle_state,
        trickle_signal,
        did_receive_packet,
        bl_state,
        flash,
        flash_channel,
    ) = STATE.init((
        comm_state,
        trickle_state,
        trickle_signal,
        did_receive_packet,
        bl_state,
        flash,
        flash_channel,
    ));

    executor.run(|spawner| {
        spawner.spawn(branch_task(did_receive_packet, bl_state, flash).unwrap());
        spawner.spawn(led_pwr_task(led_pwr).unwrap());
        spawner.spawn(led_task(leds, bl_state, watchdog).unwrap());
        spawner.spawn(flash_writer_task(flash_channel, flash, bl_state).unwrap());
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
                    flash_channel,
                    flash,
                )
                .unwrap(),
            );
        }
        spawner.spawn(tx_task(usarts_tx, comm_state, trickle_state, trickle_signal).unwrap());
    });
}
