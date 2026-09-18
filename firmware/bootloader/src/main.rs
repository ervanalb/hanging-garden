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
use hal::{Flash, Hardware, UsartRx, UsartTx};
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
unsafe fn validate_and_branch_to_app(bl_state: &mut BlState) {
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

struct FlashWriteMsg {
    chunk_index: u32,
    chunk_count: u32,
    chunk_data: heapless::Vec<u8, 256>,
}

static STATE: StaticCell<(
    Mutex<NoopRawMutex, CommState>,
    Mutex<NoopRawMutex, TrickleState>,
    Signal<NoopRawMutex, ()>,
    Mutex<NoopRawMutex, bool>,
    Mutex<NoopRawMutex, BlState>,
    Mutex<NoopRawMutex, Flash>,
    Channel<NoopRawMutex, FlashWriteMsg, 1>,
)> = StaticCell::new();

enum BlState {
    Init,
    Ping(u64),
    CodeWrite {
        chunk_index: u32,
        chunk_count: u32,
        stalled: bool,
        crc32_digest: crc::Digest<'static, u32>,
    },
    InvalidApp,
}

#[embassy_executor::task]
async fn branch_task(
    did_receive_packet: &'static Mutex<NoopRawMutex, bool>,
    bl_state: &'static Mutex<NoopRawMutex, BlState>,
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
                validate_and_branch_to_app(&mut *bl_state_guard);
            }
        }
    }
    // Wait forever
    core::future::pending::<()>().await;
}

#[embassy_executor::task]
async fn led_task(mut leds: hal::Leds, bl_state: &'static Mutex<NoopRawMutex, BlState>) {
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
                    chunk_count,
                    stalled,
                    ..
                } => {
                    // Calculate progress percentage and number of LEDs to light up
                    let progress_leds = if chunk_count > 0 {
                        ((chunk_index as u64 * 10) / chunk_count as u64).min(10) as usize
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

        // Wait until next frame time
        Timer::after_millis(33).await;
    }
}

#[embassy_executor::task]
async fn flash_writer_task(
    flash_channel: &'static Channel<NoopRawMutex, FlashWriteMsg, 1>,
    flash: &'static Mutex<NoopRawMutex, Flash>,
    bl_state: &'static Mutex<NoopRawMutex, BlState>,
) {
    let app_flash_start = unsafe { (&_sapp_usr as *const ()) as u32 };

    loop {
        let msg = flash_channel.receive().await;

        defmt::debug!("Receive chunk");

        let (mut chunk_index, mut crc32_digest) = {
            let bl_state = bl_state
                .try_lock()
                .expect("bl_state lock should not be held across .awaits");
            match &*bl_state {
                BlState::CodeWrite {
                    chunk_index,
                    crc32_digest,
                    ..
                } => (*chunk_index, crc32_digest.clone()),
                _ => (0, CRC.digest()),
            }
        };

        let stalled;
        if msg.chunk_index == chunk_index {
            // Check if non-final chunks are full (256 bytes)
            let is_last_chunk = msg.chunk_index == msg.chunk_count;

            if msg.chunk_data.len() == 256 || is_last_chunk {
                let mut flash = flash
                    .try_lock()
                    .expect("flash lock should not be held across .awaits");
                let address = app_flash_start + 256 * msg.chunk_index;
                flash.write_page(address, &msg.chunk_data).await;

                // Update CRC with the written chunk data
                crc32_digest.update(&msg.chunk_data);

                chunk_index = msg.chunk_index + 1;
                stalled = false;
                defmt::info!("Write OK!");

                // Check if this was the last chunk
                if chunk_index == msg.chunk_count {
                    let firmware_crc32 = crc32_digest.clone().finalize();
                    let firmware_size_bytes = msg.chunk_count * 256;

                    defmt::info!(
                        "Firmware complete! Size: {} bytes, CRC32: 0x{:08X}",
                        firmware_size_bytes,
                        firmware_crc32
                    );

                    // Create the bootloader config struct
                    let config = BootloaderConfig {
                        magic: BOOTLOADER_CONFIG_MAGIC,
                        firmware_size_bytes,
                        firmware_crc32,
                    };

                    // Write the config to flash at BOOTLOADER_CONFIG_USR address
                    let config_bytes = unsafe {
                        core::slice::from_raw_parts(
                            &config as *const BootloaderConfig as *const u8,
                            core::mem::size_of::<BootloaderConfig>(),
                        )
                    };

                    let config_address = &BOOTLOADER_CONFIG as *const BootloaderConfig as u32;
                    flash.write_page(config_address, config_bytes).await;
                    defmt::info!("Bootloader config written to flash");
                }
            } else {
                // Non-final chunk is not full, mark as stalled
                stalled = true;
                defmt::warn!(
                    "Chunk {} is not full ({} bytes), expected 256",
                    msg.chunk_index,
                    msg.chunk_data.len()
                );
            }
        } else if msg.chunk_index > chunk_index {
            stalled = true;
            defmt::info!("We have fallen behind");
        } else {
            stalled = false;
            defmt::info!("We are ahead");
        }
        let mut bl_state = bl_state
            .try_lock()
            .expect("bl_state lock should not be held across .awaits");
        *bl_state = BlState::CodeWrite {
            chunk_index,
            chunk_count: msg.chunk_count,
            stalled,
            crc32_digest,
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
    flash_channel: &'static Channel<NoopRawMutex, FlashWriteMsg, 1>,
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
                                            validate_and_branch_to_app(&mut *bl_state);
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
                                        let _ = flash_channel.try_send(FlashWriteMsg {
                                            chunk_index: bl_code_write.chunk_index,
                                            chunk_count: bl_code_write.chunk_count,
                                            chunk_data: bl_code_write.chunk_data.clone(),
                                        });
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
    } = Hardware::init();

    defmt::info!("Bootloader started");

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
        spawner.spawn(branch_task(did_receive_packet, bl_state).unwrap());
        spawner.spawn(led_task(leds, bl_state).unwrap());
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
                )
                .unwrap(),
            );
        }
        spawner.spawn(tx_task(usarts_tx, comm_state, trickle_state, trickle_signal).unwrap());
    });
}
