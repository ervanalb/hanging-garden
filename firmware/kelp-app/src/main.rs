#![no_std]
#![no_main]

use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use hal::{Direction, Flash, Hardware, LedPwr, Leds, Touch, UsartRx, UsartTx, Watchdog};
use proto::{CommState, CommType, MAX_PACKET_LEN, MergeResult, TRICKLE_PARAMS, Waves};
use static_cell::StaticCell;
use trickle::{TricklePollResult, TrickleState};

static EXECUTOR: StaticCell<embassy_executor::Executor> = StaticCell::new();

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

static GLOBAL_STATE: StaticCell<(
    Mutex<NoopRawMutex, AppState>,
    Signal<NoopRawMutex, ()>,
    Config,
)> = StaticCell::new();

struct AppState {
    t: i32,
    origin: bool,
}

#[derive(defmt::Format)]
#[repr(C)]
struct Config {
    magic: u32,
    pixel_count: u32,
}

/// Configuration structure placed in APP_CONFIG_USR section
#[unsafe(link_section = ".app_config_usr")]
#[used]
static CONFIG: Config = Config {
    magic: 0,
    pixel_count: 0,
};

const CONFIG_MAGIC: u32 = 0x9ad69d84;

impl Config {
    fn read_volatile(ptr: *const Config) -> Option<Self> {
        let config = unsafe { core::ptr::read_volatile(ptr) };
        if config.magic != CONFIG_MAGIC {
            defmt::warn!("Invalid magic number in config: 0x{:08X}", config.magic);
            return None;
        }
        if config.pixel_count > MAX_PIXEL_COUNT as u32 {
            defmt::warn!(
                "Invalid pixel count in config: 0x{:08X}",
                config.pixel_count
            );
            return None;
        }
        Some(config)
    }
}

const MAX_PIXEL_COUNT: usize = 120;

/// Convert HSV to RGB color space using integer math
/// h: hue [0, 1535] representing 0-360 degrees scaled by 256/60
/// s: saturation [0, 255]
/// v: value [0, 255]
/// Returns (r, g, b) in [0, 255]
fn hsv2rgb(h: u16, s: u8, v: u8) -> (u8, u8, u8) {
    // Wrap hue to [0, 1536)
    let h = h % 1536;

    let region = h / 256;
    let remainder = (h % 256) as u8;

    let p = (v as u16 * (255 - s) as u16 / 255) as u8;
    let q = (v as u16 * (255 - (s as u16 * remainder as u16 / 255)) / 255) as u8;
    let t = (v as u16 * (255 - (s as u16 * (255 - remainder) as u16 / 255)) / 255) as u8;

    match region {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

fn gamma_correct(rgb: (u8, u8, u8)) -> (u8, u8, u8) {
    let (r, g, b) = rgb;
    (r, g / 4, b / 4)
}

struct TouchState {
    last: u16,
    last_touched: bool,
}

impl TouchState {
    fn new() -> Self {
        TouchState {
            last: u16::MAX,
            last_touched: false,
        }
    }

    async fn update(&mut self, touch: &mut Touch) -> bool {
        const BASE: u32 = 16384;
        const UP_ALPHA: f32 = 0.1;
        const DOWN_ALPHA: f32 = 0.01;
        const THRESH_TOUCH: f32 = 0.6;
        const THRESH_RELEASE: f32 = 0.9;

        let val = touch.read().await;

        if self.last >= 4096 {
            self.last = val;
            self.last_touched = false;
            return false;
        }

        let alpha: u16 = if val > self.last {
            const { (UP_ALPHA * BASE as f32) as u16 }
        } else {
            const { (DOWN_ALPHA * BASE as f32) as u16 }
        };

        self.last = (self.last as i32
            + alpha as i32 * (val as i32 - self.last as i32) as i32 / BASE as i32)
            as u16;

        let thresh = if self.last_touched {
            const { (THRESH_RELEASE * BASE as f32) as u16 }
        } else {
            const { (THRESH_TOUCH * BASE as f32) as u16 }
        };

        let touched = val < (self.last as u32 * thresh as u32 / BASE) as u16;
        let newly_touched = touched && !self.last_touched;
        self.last_touched = touched;

        newly_touched
    }
}

#[embassy_executor::task]
async fn main_task(
    mut leds: Leds,
    mut touch: Touch,
    watchdog: Watchdog,
    config: &'static Config,
    app_state: &'static Mutex<NoopRawMutex, AppState>,
    directions: [(Direction, &'static DirectionState); 4],
) {
    const FRAME_INTERVAL_MS: u64 = 16; // ~1/60th of a second

    let mut buffer = [0u8; MAX_PIXEL_COUNT * 3];
    let mut time_offset: u16 = 0;

    let mut touch_state = TouchState::new();

    Timer::after_millis(100).await; // Avoid startup state transients that may flash the LEDs
    loop {
        let newly_touched = touch_state.update(&mut touch).await;

        if newly_touched {
            let mut app_state = app_state
                .try_lock()
                .expect("app_state lock should not be held across .awaits");

            app_state.t = -50;
            app_state.origin = true;

            propagate_internal_comm_state_change(
                |_direction, comm_state| {
                    comm_state.seq_num += 1;
                    comm_state.type_ = CommType::Waves(Waves { radius: 1 })
                },
                directions,
                Instant::now(),
            );
        }

        // Generate rainbow pattern
        {
            let mut app_state = app_state
                .try_lock()
                .expect("app_state lock should not be held across .awaits");
            for pixel in 0..config.pixel_count {
                let (r, g, b) = gamma_correct(if app_state.t < 0 && !app_state.origin {
                    (0, 0, 0)
                } else {
                    let wave_center = app_state.t.abs();
                    if (pixel as i32) >= wave_center - 5 && (pixel as i32) < wave_center + 5 {
                        (0, 0, 80)
                    } else {
                        (0, 0, 0)
                    }
                });

                buffer[(pixel * 3) as usize] = g;
                buffer[(pixel * 3 + 1) as usize] = r;
                buffer[(pixel * 3 + 2) as usize] = b;
            }

            app_state.t += 1;
        }

        let _ = leds.write_slice(&buffer);

        // Update time offset for next frame
        time_offset = (time_offset + 16) % 1536;

        watchdog.feed();
        Timer::after_millis(FRAME_INTERVAL_MS).await;
    }
}

#[embassy_executor::task(pool_size = 4)]
async fn rx_task(
    direction: Direction,
    mut usart_rx: UsartRx,
    direction_state: &'static DirectionState,
    other_direction_states: [(Direction, &'static DirectionState); 3],
    app_state: &'static Mutex<NoopRawMutex, AppState>,
    bootloader_signal: &'static Signal<NoopRawMutex, ()>,
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
                            // and/or the system state (app_state)
                            {
                                let mut app_state = app_state
                                    .try_lock()
                                    .expect("app_state lock should not be held across .awaits");
                                handle_new_message(
                                    direction,
                                    &mut *comm_state,
                                    &mut *app_state,
                                    bootloader_signal,
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

                                    let mut new_comm_state = comm_state.clone();
                                    propagate_one_external_change(
                                        &mut new_comm_state,
                                        Some(direction),
                                        other_direction,
                                    );
                                    let merge_result = other_comm_state.merge(&new_comm_state);

                                    if merge_result.newer {
                                        let mut other_trickle_state =
                                            other_direction_state.trickle_state.try_lock().expect(
                                                "trickle lock should not be held across .awaits",
                                            );
                                        other_trickle_state.got_inconsistent_state(now);
                                        // Wake the event loop
                                        other_direction_state.trickle_signal.signal(());
                                    }
                                }
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
    app_state: &mut AppState,
    bootloader_signal: &'static Signal<NoopRawMutex, ()>,
) {
    // Handle updating app_state
    // and altering the received state, if needed

    let CommState { seq_num: _, type_ } = &mut *comm_state;
    match type_ {
        CommType::Waves(waves) => {
            app_state.t = -50 - waves.radius as i32 * 5;
            app_state.origin = false;
        }
        CommType::BlUnknown => {
            // Reboot into bootloader
            bootloader_signal.signal(());
        }
        _ => {}
    }
}

fn propagate_one_external_change(
    comm_state: &mut CommState,
    _from_dir: Option<Direction>,
    _to_dir: Direction,
) {
    match &mut comm_state.type_ {
        CommType::Waves(waves) => {
            waves.radius += 1;
        }
        _ => {}
    }
}

fn propagate_internal_comm_state_change(
    propagate: impl Fn(Direction, &mut CommState),
    direction_states: [(Direction, &'static DirectionState); 4],
    now: Instant,
) {
    for (direction, direction_state) in direction_states {
        let mut other_comm_state = direction_state
            .comm_state
            .try_lock()
            .expect("comm_state lock should not be held across .awaits");

        let mut comm_state = other_comm_state.clone();
        propagate(direction, &mut comm_state);
        let merge_result = other_comm_state.merge(&comm_state);

        if merge_result.newer {
            let mut trickle_state = direction_state
                .trickle_state
                .try_lock()
                .expect("trickle lock should not be held across .awaits");
            trickle_state.got_inconsistent_state(now);
            // Wake the event loop
            direction_state.trickle_signal.signal(());
        }
    }
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

#[embassy_executor::task]
async fn led_pwr_task(mut led_pwr: hal::LedPwr) {
    loop {
        // Set LED power on
        led_pwr.set_pwr(true);
        Timer::after_millis(100).await;

        loop {
            // Read current at 100Hz (every 10ms)
            Timer::after_millis(10).await;
            let current = led_pwr.read_current().await;

            if current > 150 {
                defmt::info!("Overcurrent detected: {}, shutting off LEDs", current);
                break;
            }
        }

        // Shut off LED power
        led_pwr.set_pwr(false);

        // Wait 1 second
        Timer::after_secs(1).await;

        defmt::info!("LEDs powered back on");
    }
}

#[embassy_executor::task]
async fn branch_task(bootloader_signal: &'static Signal<NoopRawMutex, ()>) {
    // Wait for signal from handle_new_message
    loop {
        bootloader_signal.wait().await;
        Timer::after_millis(80).await; // Wait 80ms for propagation of message
        // Reboot into bootloader
        // Safety: we are not in an interrupt
        unsafe {
            hal::branch_to_bootloader();
        }
    }
}

async fn get_or_discover_config(
    flash: &mut Flash,
    leds: &mut Leds,
    led_pwr: &mut LedPwr,
    watchdog: &Watchdog,
) -> Config {
    // Read the config from flash if it is valid
    if let Some(config) = Config::read_volatile(&CONFIG) {
        return config;
    }

    // Count pixels using current measurement
    const AVG_COUNT: usize = 128;
    let mut buffer = [0u8; (MAX_PIXEL_COUNT + 1) * 3];
    led_pwr.set_pwr(true);
    Timer::after_millis(100).await;
    let _ = leds.write_slice(&buffer);
    Timer::after_millis(10).await;
    let mut current_dark: u32 = 0;
    for _ in 0..AVG_COUNT {
        current_dark += led_pwr.read_current().await as u32;
    }
    watchdog.feed();
    // Set first pixel to all white
    buffer[0..3].clone_from_slice(&[0xFF; 3]);
    let _ = leds.write_slice(&buffer);
    buffer[0..3].clone_from_slice(&[0x00; 3]);
    Timer::after_millis(10).await;
    let mut current_light: u32 = 0;
    for _ in 0..AVG_COUNT {
        current_light += led_pwr.read_current().await as u32;
    }
    watchdog.feed();

    const RADIUS: u32 = (AVG_COUNT as u32) / 2; // 0.5 ADC count

    // Require a minimum difference of 3 * RADIUS
    if current_light < current_dark + (3 * RADIUS) || current_dark < RADIUS {
        defmt::error!(
            "Bad current readings (dark: {}, light: {})",
            current_dark,
            current_light
        );
        return Config {
            magic: CONFIG_MAGIC,
            pixel_count: 10,
        };
    }

    // Linear search for the end of the strip
    let mut pixel_count = 1;
    for i in 1..MAX_PIXEL_COUNT {
        // Set given pixel to all white
        buffer[i * 3..i * 3 + 3].clone_from_slice(&[0xFF; 3]);
        let _ = leds.write_slice(&buffer);
        buffer[i * 3..i * 3 + 3].clone_from_slice(&[0x00; 3]);
        Timer::after_millis(10).await;
        let mut current: u32 = 0;
        for _ in 0..AVG_COUNT {
            current += led_pwr.read_current().await as u32;
        }
        watchdog.feed();

        if current >= current_dark - RADIUS && current < current_dark + RADIUS {
            // Found a dark pixel--stop counting
            break;
        } else if current >= current_light - RADIUS && current < current_light + RADIUS {
            pixel_count = i as u32 + 1;
            // Continue counting
        } else {
            // Pixel is not dark or light--error
            defmt::error!(
                "Bad current reading of {} (dark: {}, light: {})",
                current,
                current_dark,
                current_light
            );
            return Config {
                magic: CONFIG_MAGIC,
                pixel_count: 10,
            };
        }
    }

    // Confirm 3 times that we correctly found the end of the strip
    for _ in 0..3 {
        // Set last pixel to all white
        buffer[3 * (pixel_count - 1) as usize..3 * pixel_count as usize]
            .clone_from_slice(&[0xFF; 3]);
        let _ = leds.write_slice(&buffer);
        buffer[3 * (pixel_count - 1) as usize..3 * pixel_count as usize]
            .clone_from_slice(&[0x00; 3]);
        Timer::after_millis(10).await;
        let mut current: u32 = 0;
        for _ in 0..AVG_COUNT {
            current += led_pwr.read_current().await as u32;
        }
        if !(current >= current_light - RADIUS && current < current_light + RADIUS) {
            // Pixel was expected to be light, but it was not
            defmt::error!(
                "Bad confirmation reading of {} (expected light: {})",
                current,
                current_light
            );
            return Config {
                magic: CONFIG_MAGIC,
                pixel_count: 10,
            };
        }

        // Set pixel after the last to all white
        buffer[3 * pixel_count as usize..3 * (pixel_count as usize + 1)]
            .clone_from_slice(&[0xFF; 3]);
        let _ = leds.write_slice(&buffer);
        buffer[3 * pixel_count as usize..3 * (pixel_count as usize + 1)]
            .clone_from_slice(&[0x00; 3]);
        Timer::after_millis(10).await;
        let mut current: u32 = 0;
        for _ in 0..AVG_COUNT {
            current += led_pwr.read_current().await as u32;
        }
        if !(current >= current_dark - RADIUS && current < current_dark + RADIUS) {
            // Pixel was expected to be dark, but it was not
            defmt::error!(
                "Bad confirmation reading of {} (expected dark: {})",
                current,
                current_dark
            );
            return Config {
                magic: CONFIG_MAGIC,
                pixel_count: 10,
            };
        }

        watchdog.feed();
    }

    let config = Config {
        magic: CONFIG_MAGIC,
        pixel_count,
    };

    defmt::info!("Writing new config: {}", &config);

    let config_bytes = unsafe {
        core::slice::from_raw_parts(
            &config as *const Config as *const u8,
            core::mem::size_of::<Config>(),
        )
    };

    let config_address = &CONFIG as *const Config as u32;
    flash.write_page(config_address, config_bytes).await;
    defmt::info!("Config written to flash");

    config
}

#[embassy_executor::task]
async fn init(
    spawner: embassy_executor::Spawner,
    mut led_pwr: LedPwr,
    mut leds: Leds,
    touch: Touch,
    watchdog: Watchdog,
    chip_id: [u32; 3],
    mut flash: Flash,
    north_tx: UsartTx,
    south_tx: UsartTx,
    east_tx: UsartTx,
    west_tx: UsartTx,
    north_rx: UsartRx,
    south_rx: UsartRx,
    east_rx: UsartRx,
    west_rx: UsartRx,
) {
    let config = get_or_discover_config(&mut flash, &mut leds, &mut led_pwr, &watchdog).await;

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
    let app_state = Mutex::new(AppState { t: 0, origin: false });
    let bootloader_signal = Signal::new();
    let (app_state, bootloader_signal, config) =
        GLOBAL_STATE.init((app_state, bootloader_signal, config));

    spawner.spawn(led_pwr_task(led_pwr).unwrap());
    spawner.spawn(
        main_task(
            leds,
            touch,
            watchdog,
            config,
            app_state,
            [
                (Direction::North, north_state),
                (Direction::South, south_state),
                (Direction::East, east_state),
                (Direction::West, west_state),
            ],
        )
        .unwrap(),
    );
    spawner.spawn(branch_task(bootloader_signal).unwrap());

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
            app_state,
            bootloader_signal,
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
            app_state,
            bootloader_signal,
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
            app_state,
            bootloader_signal,
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
            app_state,
            bootloader_signal,
        )
        .unwrap(),
    );

    // Spawn TX tasks for each direction
    spawner.spawn(tx_task(Direction::North, north_tx, north_state).unwrap());
    spawner.spawn(tx_task(Direction::South, south_tx, south_state).unwrap());
    spawner.spawn(tx_task(Direction::East, east_tx, east_state).unwrap());
    spawner.spawn(tx_task(Direction::West, west_tx, west_state).unwrap());
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
        touch,
    } = Hardware::init();

    // Create executor
    let executor = EXECUTOR.init(embassy_executor::Executor::new());

    executor.run(|spawner| {
        spawner.spawn(
            init(
                spawner, led_pwr, leds, touch, watchdog, chip_id, flash, north_tx, south_tx,
                east_tx, west_tx, north_rx, south_rx, east_rx, west_rx,
            )
            .unwrap(),
        );
    });
}
