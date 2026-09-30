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

use embassy_sync::{
    blocking_mutex::raw::NoopRawMutex, channel::Channel, mutex::Mutex, signal::Signal,
};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use hal::{Direction, Flash, HARDWARE_ID, Hardware, UsartRx, UsartTx, Watchdog};
use proto::{CommState, CommType, MAX_PACKET_LEN, MergeResult, TRICKLE_PARAMS};
use static_cell::StaticCell;
use trickle::{TricklePollResult, TrickleState};

static CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_BZIP2);

static EXECUTOR: StaticCell<embassy_executor::Executor> = StaticCell::new();

const BOOTLOADER_CONFIG_MAGIC: u32 = 0x7ac37303;

/// Validates the application firmware using the CRC32 from BOOTLOADER_CONFIG and branches to app if valid.
///
/// # Safety
/// This function may branch to application code.
/// It should only be called when not in an interrupt.
unsafe fn validate_and_branch_to_app(bl_state: &mut BlState, _flash: &mut Flash) {
    // Read the config from flash
    // Safety: BOOTLOADER_CONFIG is at a fixed, valid memory location
    let config_ptr = &BOOTLOADER_CONFIG as *const BootloaderConfig;
    let config = unsafe { core::ptr::read_volatile(config_ptr) };

    // Check magic number
    if config.magic != BOOTLOADER_CONFIG_MAGIC {
        defmt::warn!("Invalid magic number in config: 0x{:08X}", config.magic);
        *bl_state = BlState::IndicateBad;
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
        *bl_state = BlState::IndicateBad;
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
        *bl_state = BlState::IndicateBad;
    }
}

// Direction-specific state (one per neighbor direction)
struct DirectionState {
    comm_state: Mutex<NoopRawMutex, CommState>,
    trickle_state: Mutex<NoopRawMutex, TrickleState<'static>>,
    trickle_signal: Signal<NoopRawMutex, ()>,
}

static NORTH_STATE: StaticCell<DirectionState> = StaticCell::new();
static SOUTH_STATE: StaticCell<DirectionState> = StaticCell::new();
static EAST_STATE: StaticCell<DirectionState> = StaticCell::new();
static WEST_STATE: StaticCell<DirectionState> = StaticCell::new();

// Shared state across all directions
static GLOBAL_STATE: StaticCell<(
    Mutex<NoopRawMutex, bool>,
    Mutex<NoopRawMutex, BlState>,
    Mutex<NoopRawMutex, Flash>,
    Channel<NoopRawMutex, proto::BlCodeWrite, 1>,
    Signal<NoopRawMutex, ()>,
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
    IndicateGood,
    IndicateBad,
}

#[embassy_executor::task]
async fn branch_task(
    did_receive_packet: &'static Mutex<NoopRawMutex, bool>,
    bl_state: &'static Mutex<NoopRawMutex, BlState>,
    flash: &'static Mutex<NoopRawMutex, Flash>,
    validate_signal: &'static Signal<NoopRawMutex, ()>,
) {
    use embassy_futures::select::{Either, select};

    // Wait for either 500ms timeout or validation signal
    match select(Timer::after_millis(500), validate_signal.wait()).await {
        Either::First(_) => {
            // Timer expired - check if we should auto-validate
            let did_receive_packet = {
                let did_receive_packet = did_receive_packet
                    .try_lock()
                    .expect("did_receive_packet should not be held across .awaits");
                *did_receive_packet
            };
            if !did_receive_packet {
                // Safety: we are not in an interrupt
                let mut flash_guard = flash.lock().await;
                let mut bl_state_guard = bl_state
                    .try_lock()
                    .expect("bl_state lock should not be held across .awaits");
                unsafe {
                    validate_and_branch_to_app(&mut *bl_state_guard, &mut *flash_guard);
                }
            }
        }
        Either::Second(_) => {
            // Validation signal received during initial 500ms
            Timer::after_millis(80).await; // Wait 80ms for propagation of message
            // Safety: we are not in an interrupt
            let mut flash_guard = flash.lock().await;
            let mut bl_state_guard = bl_state
                .try_lock()
                .expect("bl_state lock should not be held across .awaits");
            unsafe {
                validate_and_branch_to_app(&mut *bl_state_guard, &mut *flash_guard);
            }
        }
    }

    // Wait for validation signals from handle_new_message
    loop {
        validate_signal.wait().await;
        Timer::after_millis(80).await; // Wait 80ms for propagation of message
        // Safety: we are not in an interrupt
        let mut flash_guard = flash.lock().await;
        let mut bl_state_guard = bl_state
            .try_lock()
            .expect("bl_state lock should not be held across .awaits");
        unsafe {
            validate_and_branch_to_app(&mut *bl_state_guard, &mut *flash_guard);
        }
    }
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
                BlState::IndicateGood => {
                    // Display green
                    for i in 0..10 {
                        led_pattern[i * 3] = 0x10; // G
                        led_pattern[i * 3 + 1] = 0x00; // R
                        led_pattern[i * 3 + 2] = 0x00; // B
                    }
                }
                BlState::IndicateBad => {
                    // Display red
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
                msg.chunk_index,
                chunk_index
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
    direction: Direction,
    mut usart_rx: UsartRx,
    direction_state: &'static DirectionState,
    other_direction_states: [(Direction, &'static DirectionState); 3],
    did_receive_packet: &'static Mutex<NoopRawMutex, bool>,
    bl_state: &'static Mutex<NoopRawMutex, BlState>,
    flash_channel: &'static Channel<NoopRawMutex, proto::BlCodeWrite, 1>,
    validate_signal: &'static Signal<NoopRawMutex, ()>,
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
                    defmt::trace!("RX from {} framed bytes: {:?}", direction, &rx_buffer[..=i]);
                    if let Ok(received_comm_state) =
                        CommState::try_deserialize_packet(&mut rx_buffer[..=i]).map_err(|e| {
                            defmt::debug!("RX err: {:?}", defmt::Debug2Format(&e));
                            e
                        })
                    {
                        let now = Instant::now();
                        defmt::debug!("RX {}: packet {:?}", direction, received_comm_state);
                        // We got a valid packet--update the state

                        let mut comm_state = direction_state
                            .comm_state
                            .try_lock()
                            .expect("comm_state lock should not be held across .awaits");
                        let merge_result = comm_state.merge(&received_comm_state);

                        if merge_result.newer {
                            // Update the message itself,
                            // and/or the system state (bl_state)
                            {
                                let mut bl_state = bl_state
                                    .try_lock()
                                    .expect("bl_state lock should not be held across .awaits");
                                handle_new_message(
                                    direction,
                                    &mut *comm_state,
                                    &mut *bl_state,
                                    flash_channel,
                                    validate_signal,
                                );
                            }

                            // Handle propagation
                            // (including updating trickle algorithm for other directions)
                            {
                                for (other_direction, other_direction_state) in
                                    other_direction_states
                                {
                                    let mut other_comm_state =
                                        other_direction_state.comm_state.try_lock().expect(
                                            "comm_state lock should not be held across .awaits",
                                        );

                                    propagate_comm_state(
                                        direction,
                                        &mut *comm_state,
                                        other_direction,
                                        &mut *other_comm_state,
                                    );
                                    // Bootloader has very simple propagation:
                                    // clone an exact copy to all other directions
                                    *other_comm_state = comm_state.clone();

                                    let mut other_trickle_state = other_direction_state
                                        .trickle_state
                                        .try_lock()
                                        .expect("trickle lock should not be held across .awaits");
                                    other_trickle_state.got_inconsistent_state(now);
                                    // Wake the event loop
                                    other_direction_state.trickle_signal.signal(());
                                }
                            }
                            {
                                let mut did_receive_packet = did_receive_packet
                                    .try_lock()
                                    .expect("did_receive_packet should not be held across .awaits");
                                *did_receive_packet = true;
                            }
                        }

                        // Update the trickle algorithm for this direction
                        {
                            let mut trickle_state = direction_state
                                .trickle_state
                                .try_lock()
                                .expect("trickle lock should not be held across .awaits");
                            if merge_result == MergeResult::CONSISTENT {
                                trickle_state.got_consistent_state();
                            } else {
                                trickle_state.got_inconsistent_state(now);
                                // Wake the event loop
                                direction_state.trickle_signal.signal(());
                            }
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
            rx_buffer.clear();
            defmt::info!("{} Overrun", direction);
        }
    }
}

fn handle_new_message(
    _direction: Direction,
    comm_state: &mut CommState,
    bl_state: &mut BlState,
    flash_channel: &'static Channel<NoopRawMutex, proto::BlCodeWrite, 1>,
    validate_signal: &'static Signal<NoopRawMutex, ()>,
) {
    // Handle updating bl_state
    // and altering the received state, if needed

    let CommState { seq_num, type_ } = &mut *comm_state;
    match type_ {
        CommType::Unknown => {
            // Validate and reboot into app
            // Signal branch_task to validate and branch to app
            validate_signal.signal(());
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
                    bl_broadcast_ping.latency_micros = bl_broadcast_ping.age_micros.age_micros;
                    *bl_state = BlState::Ping(comm_state.seq_num)
                }
            }
        }
        CommType::BlCodeWrite(bl_code_write) => {
            // Send the flash write message to the flash writer task
            // if this message is for us (hardware ID matches)
            if bl_code_write.hardware_id == HARDWARE_ID {
                let _ = flash_channel.try_send(bl_code_write.clone());
            }
        }
        CommType::BlCodeProgress(bl_code_progress) => {
            let chunk_index = match *bl_state {
                BlState::CodeWrite { chunk_index, .. } => chunk_index,
                _ => 0,
            };
            bl_code_progress.chunk_count = bl_code_progress.chunk_count.min(chunk_index);
        }
        CommType::BlIndicateGood => {
            *bl_state = BlState::IndicateGood;
        }
        CommType::BlUnknown => {}
    }
}

fn propagate_comm_state(
    _from_dir: Direction,
    from_comm_state: &CommState,
    _to_dir: Direction,
    to_comm_state: &mut CommState,
) -> bool {
    *to_comm_state = from_comm_state.clone();
    true
}

#[embassy_executor::task(pool_size = 4)]
async fn tx_task(
    direction: Direction,
    mut usart_tx: UsartTx,
    direction_state: &'static DirectionState,
) {
    let mut tx_buffer = [0_u8; MAX_PACKET_LEN + 1];
    loop {
        let now = Instant::now();

        let mut trickle_state = direction_state
            .trickle_state
            .try_lock()
            .expect("trickle lock should not be held across .awaits");
        match trickle_state.poll(now) {
            TricklePollResult::Wait(timeout_micros) => {
                // drop the lock before .await
                drop(trickle_state);

                // Wait for the alotted time,
                // or until we are interrupted from rx_task
                let _ = direction_state
                    .trickle_signal
                    .wait()
                    .with_timeout(Duration::from_micros(timeout_micros))
                    .await;
            }
            TricklePollResult::Send => {
                let mut comm_state = direction_state
                    .comm_state
                    .try_lock()
                    .expect("comm_state lock should not be held across .awaits");
                comm_state.update(now);

                defmt::debug!("TX {}: {:?}", direction, &*comm_state);

                // We retain an initial '\0' to improve packet start detection
                let len = comm_state.serialize_packet(&mut tx_buffer[1..]).len() + 1;

                // drop the locks before .await
                drop(trickle_state);
                drop(comm_state);

                usart_tx.write(&tx_buffer[..len]).await;
            }
        }
    }
}

#[qingke_rt::entry]
fn main() -> ! {
    let Hardware {
        leds,
        led_pwr,
        usarts_tx: [north_tx, south_tx, east_tx, west_tx],
        usarts_rx: [north_rx, south_rx, east_rx, west_rx],
        flash,
        watchdog,
        chip_id,
        touch: _,
    } = Hardware::init();

    defmt::info!("Bootloader started");

    // Create executor
    let executor = EXECUTOR.init(embassy_executor::Executor::new());

    // Seed RNG with unique chip identifier
    let seed = (((chip_id[0] as u64) << 32) | (chip_id[1] as u64)) ^ (chip_id[2] as u64);
    let now = Instant::now();

    // Initialize direction-specific states
    let north_state = NORTH_STATE.init(DirectionState {
        comm_state: Mutex::new(CommState::default()),
        trickle_state: Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, seed)),
        trickle_signal: Signal::new(),
    });

    let south_state = SOUTH_STATE.init(DirectionState {
        comm_state: Mutex::new(CommState::default()),
        trickle_state: Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, seed ^ 1)),
        trickle_signal: Signal::new(),
    });

    let east_state = EAST_STATE.init(DirectionState {
        comm_state: Mutex::new(CommState::default()),
        trickle_state: Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, seed ^ 2)),
        trickle_signal: Signal::new(),
    });

    let west_state = WEST_STATE.init(DirectionState {
        comm_state: Mutex::new(CommState::default()),
        trickle_state: Mutex::new(TrickleState::new(&TRICKLE_PARAMS, now, seed ^ 3)),
        trickle_signal: Signal::new(),
    });

    // Initialize shared state
    let did_receive_packet = Mutex::new(false);
    let bl_state = Mutex::new(BlState::Init);
    let flash = Mutex::new(flash);
    let flash_channel = Channel::new();
    let validate_signal = Signal::new();
    let (did_receive_packet, bl_state, flash, flash_channel, validate_signal) =
        GLOBAL_STATE.init((
            did_receive_packet,
            bl_state,
            flash,
            flash_channel,
            validate_signal,
        ));

    executor.run(|spawner| {
        spawner.spawn(branch_task(did_receive_packet, bl_state, flash, validate_signal).unwrap());
        spawner.spawn(led_pwr_task(led_pwr).unwrap());
        spawner.spawn(led_task(leds, bl_state, watchdog).unwrap());
        spawner.spawn(flash_writer_task(flash_channel, flash, bl_state).unwrap());

        // Spawn RX tasks for each direction
        spawner.spawn(
            rx_task(
                Direction::North,
                north_rx,
                north_state,
                [
                    (Direction::South, south_state),
                    (Direction::East, east_state),
                    (Direction::West, west_state),
                ],
                did_receive_packet,
                bl_state,
                flash_channel,
                validate_signal,
            )
            .unwrap(),
        );
        spawner.spawn(
            rx_task(
                Direction::South,
                south_rx,
                south_state,
                [
                    (Direction::North, north_state),
                    (Direction::East, east_state),
                    (Direction::West, west_state),
                ],
                did_receive_packet,
                bl_state,
                flash_channel,
                validate_signal,
            )
            .unwrap(),
        );
        spawner.spawn(
            rx_task(
                Direction::East,
                east_rx,
                east_state,
                [
                    (Direction::North, north_state),
                    (Direction::South, south_state),
                    (Direction::West, west_state),
                ],
                did_receive_packet,
                bl_state,
                flash_channel,
                validate_signal,
            )
            .unwrap(),
        );
        spawner.spawn(
            rx_task(
                Direction::West,
                west_rx,
                west_state,
                [
                    (Direction::North, north_state),
                    (Direction::South, south_state),
                    (Direction::East, east_state),
                ],
                did_receive_packet,
                bl_state,
                flash_channel,
                validate_signal,
            )
            .unwrap(),
        );

        // Spawn TX tasks for each direction
        spawner.spawn(tx_task(Direction::North, north_tx, north_state).unwrap());
        spawner.spawn(tx_task(Direction::South, south_tx, south_state).unwrap());
        spawner.spawn(tx_task(Direction::East, east_tx, east_state).unwrap());
        spawner.spawn(tx_task(Direction::West, west_tx, west_state).unwrap());
    });
}
