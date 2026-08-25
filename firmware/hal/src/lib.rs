#![no_std]
#![allow(static_mut_refs)]

use core::cell::RefCell;
use core::future::poll_fn;
use core::sync::atomic::{AtomicU32, Ordering, compiler_fence};
use core::task::Poll;

use ch32_metapac as pac;
use core::panic::PanicInfo;
use critical_section::CriticalSection;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::waitqueue::AtomicWaker;
use embassy_time_driver::Driver;
use embassy_time_queue_utils::Queue;
use pac::dma::vals::{Dir, Pl, Size};
use pac::gpio::vals::{Cnf, Mode};
use pac::rcc::vals::{Hpre, PllMul, Ppre, Sw};
use pac::spi::vals::BaudRate;
use pac::systick::vals;

#[allow(improper_ctypes)]
unsafe extern "C" {
    static _sbootloader: ();
    static _sapp: ();
    static _sflash: ();
}

#[inline(never)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    riscv::asm::delay(1_000_000);
    use crate::println;
    println!("*** PANIC ***");
    println!("{}", info);
    loop {}
}

// Constants for SDI print
pub const DEBUG_DATA0_ADDRESS: *mut u32 = 0xE000_0380 as *mut u32;
pub const DEBUG_DATA1_ADDRESS: *mut u32 = 0xE000_0384 as *mut u32;

pub struct SDIPrint {}

impl core::fmt::Write for SDIPrint {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let mut data = [0u8; 8];
        for chunk in s.as_bytes().chunks(7) {
            data[1..chunk.len() + 1].copy_from_slice(chunk);
            data[0] = chunk.len() as u8;

            // data1 is the last 4 bytes of data
            let data1 = u32::from_le_bytes(data[4..].try_into().unwrap());
            let data0 = u32::from_le_bytes(data[..4].try_into().unwrap());

            // Wait for not busy
            unsafe { while core::ptr::read_volatile(DEBUG_DATA0_ADDRESS) != 0 {} }

            unsafe {
                core::ptr::write_volatile(DEBUG_DATA1_ADDRESS, data1);
                core::ptr::write_volatile(DEBUG_DATA0_ADDRESS, data0);
            }
        }

        Ok(())
    }
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        {
            use core::fmt::Write;
            use core::writeln;

            writeln!(&mut $crate::SDIPrint {}, $($arg)*).unwrap();
        }
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        {
            use core::fmt::Write;
            use core::write;

            write!(&mut $crate::SDIPrint {}, $($arg)*).unwrap();
        }
    }
}

pub struct SystickDriver {
    cnt_per_tick: AtomicU32,
    queue: Mutex<CriticalSectionRawMutex, RefCell<Queue>>,
}

embassy_time_driver::time_driver_impl!(static TIME_DRIVER: SystickDriver = SystickDriver {
    cnt_per_tick: AtomicU32::new(1), // avoid div by zero
    queue: Mutex::new(RefCell::new(Queue::new()))
});

impl SystickDriver {
    fn init(&'static self, _cs: CriticalSection, hclk: u32) {
        let r = &pac::SYSTICK;
        let hclk = hclk as u64;

        let cnt_per_second = hclk / 8; // HCLK/8
        let cnt_per_tick = cnt_per_second / embassy_time_driver::TICK_HZ;

        self.cnt_per_tick
            .store(cnt_per_tick as u32, Ordering::Relaxed);

        r.ctlr().write(|w| {
            w.set_init(true); // Initialize counter
            w.set_ste(true); // Enable counter
        });

        // Write 0 to both halves of the compare register
        r.cmph().write_value(0);
        r.cmpl().write_value(0);

        // Count value compare flag
        r.sr().write(|w| w.set_cntif(false)); // clear

        // Configuration: Upcount, No reload, HCLK/8 as clock source
        r.ctlr().modify(|w| {
            w.set_mode(vals::Mode::UPCOUNT); // Counter mode
            w.set_stre(false); // Auto reload count enable bit
            w.set_stclk(vals::Stclk::HCLK_DIV8); // Counter system clock source selection bit
        });
    }

    fn on_interrupt(&self) {
        let r = &pac::SYSTICK;
        // Count value compare flag
        r.sr().write(|w| w.set_cntif(false)); // clear IF

        critical_section::with(|cs| {
            self.trigger_alarm(cs);
        });
    }

    #[inline]
    fn raw_cnt(&self) -> u64 {
        let r = pac::SYSTICK;
        r.cnt().read()
    }

    fn trigger_alarm(&self, cs: CriticalSection) {
        let mut next = self
            .queue
            .borrow(cs)
            .borrow_mut()
            .next_expiration(self.raw_cnt());
        while !self.set_alarm(cs, next) {
            next = self
                .queue
                .borrow(cs)
                .borrow_mut()
                .next_expiration(self.raw_cnt());
        }
    }

    fn set_alarm(&self, _cs: CriticalSection, next_alarm_cnt: u64) -> bool {
        let r = &pac::SYSTICK;

        if next_alarm_cnt <= self.raw_cnt() {
            return false;
        }

        // Counter interrupt enable control bit
        r.cmph().write_value((next_alarm_cnt >> 32) as u32);
        r.cmpl().write_value((next_alarm_cnt) as u32);
        r.ctlr().modify(|w| w.set_stie(true));
        r.sr().write(|w| w.set_cntif(false));

        if next_alarm_cnt <= self.raw_cnt() {
            // If alarm timestamp has passed the alarm will not fire.
            // Disarm the alarm and return `false` to indicate that.
            r.ctlr().modify(|w| w.set_stie(false));
            r.sr().write(|w| w.set_cntif(false));
            return false;
        }

        true
    }
}

impl Driver for SystickDriver {
    fn now(&self) -> u64 {
        let cnt_per_tick = self.cnt_per_tick.load(Ordering::Relaxed) as u64;
        self.raw_cnt() / cnt_per_tick
    }

    fn schedule_wake(&self, ticks: u64, waker: &core::task::Waker) {
        let cnt_per_tick = self.cnt_per_tick.load(Ordering::Relaxed) as u64;
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();

            if queue.schedule_wake(ticks * cnt_per_tick, waker) {
                let mut next = queue.next_expiration(self.raw_cnt());
                while !self.set_alarm(cs, next) {
                    next = queue.next_expiration(self.raw_cnt());
                }
            }
        })
    }
}

// ============================================================================
// LED and USART driver code
// ============================================================================

const LED_FRAME_BUFFER_MAX_DATA_COUNT: usize = 1200;

struct LedFrameBuffer {
    len: usize,
    data: [u8; LED_FRAME_BUFFER_MAX_DATA_COUNT],
}

impl LedFrameBuffer {
    pub fn fill_from_slice(&mut self, data: &[u8]) {
        self.data[..data.len()].copy_from_slice(data);
        self.len = data.len();
    }

    fn next_4bits(&self, nibble_index: &mut usize) -> Option<u16> {
        if *nibble_index / 2 >= self.len {
            return None;
        }

        // Get nibble to send in first 4 bits of `b`
        let mut b = self.data[*nibble_index / 2];
        if *nibble_index % 2 == 0 {
            // Process high nibble first
            b >>= 4;
        }

        // Convert `b` to WS2812 signalling
        // This could probably be implemented with more clever bit-hacking
        let mut out = 0_u16;
        for _ in 0..4 {
            out <<= 4;
            if b & 0b1000 == 0 {
                out |= 0b1000;
            } else {
                out |= 0b1110;
            }
            b <<= 1;
        }
        *nibble_index += 1;

        Some(out)
    }
}

struct LedTx {
    region: fring::Region<'static, fring::Consumer<'static, LedFrameBuffer, 2>, LedFrameBuffer>,
    nibble_index: usize,
    reset_pulse_count: usize,
}

// Global ring buffer
static LED_FRAME_BUFFERS: fring::Buffer<LedFrameBuffer, 2> = fring::Buffer::new();
static mut LED_FRAME_BUFFERS_PRODUCER: fring::Producer<LedFrameBuffer, 2> =
    unsafe { LED_FRAME_BUFFERS.producer() };
static mut LED_FRAME_BUFFERS_CONSUMER: fring::Consumer<LedFrameBuffer, 2> =
    unsafe { LED_FRAME_BUFFERS.consumer() };

static mut LED_ACTIVE_TX: Option<LedTx> = None;

// USART TX wakers for async operations
static USART1_TX_WAKER: AtomicWaker = AtomicWaker::new();
static USART2_TX_WAKER: AtomicWaker = AtomicWaker::new();
static USART3_TX_WAKER: AtomicWaker = AtomicWaker::new();
static USART4_TX_WAKER: AtomicWaker = AtomicWaker::new();

// USART RX wakers for async operations
static USART1_RX_WAKER: AtomicWaker = AtomicWaker::new();
static USART2_RX_WAKER: AtomicWaker = AtomicWaker::new();
static USART3_RX_WAKER: AtomicWaker = AtomicWaker::new();
static USART4_RX_WAKER: AtomicWaker = AtomicWaker::new();

// USART RX circular DMA buffers
const USART_RX_DMA_BUFFER_SIZE: usize = 64;
static mut USART1_RX_DMA_BUFFER: [u8; USART_RX_DMA_BUFFER_SIZE] = [0; USART_RX_DMA_BUFFER_SIZE];
static mut USART2_RX_DMA_BUFFER: [u8; USART_RX_DMA_BUFFER_SIZE] = [0; USART_RX_DMA_BUFFER_SIZE];
static mut USART3_RX_DMA_BUFFER: [u8; USART_RX_DMA_BUFFER_SIZE] = [0; USART_RX_DMA_BUFFER_SIZE];
static mut USART4_RX_DMA_BUFFER: [u8; USART_RX_DMA_BUFFER_SIZE] = [0; USART_RX_DMA_BUFFER_SIZE];

// USART RX fring buffers
pub const USART_RX_CAPACITY: usize = 1024;
static USART1_RX_FRING: fring::Buffer<u8, USART_RX_CAPACITY> = fring::Buffer::new();
static USART2_RX_FRING: fring::Buffer<u8, USART_RX_CAPACITY> = fring::Buffer::new();
static USART3_RX_FRING: fring::Buffer<u8, USART_RX_CAPACITY> = fring::Buffer::new();
static USART4_RX_FRING: fring::Buffer<u8, USART_RX_CAPACITY> = fring::Buffer::new();

static mut USART1_RX_FRING_PRODUCER: fring::Producer<u8, USART_RX_CAPACITY> =
    unsafe { USART1_RX_FRING.producer() };
static mut USART2_RX_FRING_PRODUCER: fring::Producer<u8, USART_RX_CAPACITY> =
    unsafe { USART2_RX_FRING.producer() };
static mut USART3_RX_FRING_PRODUCER: fring::Producer<u8, USART_RX_CAPACITY> =
    unsafe { USART3_RX_FRING.producer() };
static mut USART4_RX_FRING_PRODUCER: fring::Producer<u8, USART_RX_CAPACITY> =
    unsafe { USART4_RX_FRING.producer() };

// Track last DMA read position for each USART
static mut USART1_RX_LAST_POS: usize = 0;
static mut USART2_RX_LAST_POS: usize = 0;
static mut USART3_RX_LAST_POS: usize = 0;
static mut USART4_RX_LAST_POS: usize = 0;

/// USART TX handle for async operations
pub struct UsartTx {
    dma_ch: usize,
    waker: &'static AtomicWaker,
}

impl UsartTx {
    /// Async write to USART
    pub async fn write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        assert!(data.len() <= 0xFFFF, "Buffer too large for DMA");

        // Configure DMA for this transfer
        // Set peripheral and memory addresses
        pac::DMA1
            .ch(self.dma_ch - 1)
            .mar()
            .write_value(data.as_ptr() as u32);
        pac::DMA1
            .ch(self.dma_ch - 1)
            .ndtr()
            .write(|w| w.set_ndt(data.len() as u16));

        compiler_fence(Ordering::SeqCst);

        // Start DMA transfer
        pac::DMA1
            .ch(self.dma_ch - 1)
            .cr()
            .modify(|w| w.set_en(true));

        // Wait for transfer to complete
        poll_fn(|cx| {
            self.waker.register(cx.waker());

            compiler_fence(Ordering::SeqCst);

            // Check if transfer is complete
            let cr = pac::DMA1.ch(self.dma_ch - 1).cr().read();
            if !cr.en() {
                // Transfer complete
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

/// Initialize a USART peripheral in half-duplex mode at 1 Mbaud with DMA
///
/// # Arguments
/// * `usart` - The USART peripheral to configure
/// * `gpio_port` - The GPIO port for the TX pin
/// * `gpio_pin` - The GPIO pin number for TX
/// * `dma_tx_channel` - DMA channel number for TX (0-7)
/// * `dma_rx_channel` - DMA channel number for RX (0-7)
/// * `rx_dma_buffer` - Pointer to the circular RX DMA buffer
unsafe fn init_usart_halfduplex(
    usart: pac::usart::Usart,
    gpio_port: pac::gpio::Gpio,
    gpio_pin: usize,
    dma_tx_channel: usize,
    dma_rx_channel: usize,
    rx_dma_buffer: &'static mut [u8],
) {
    // Configure GPIO pin as alternate function push-pull
    if gpio_pin >= 8 {
        gpio_port.cfghr().modify(|w| {
            w.set_mode(gpio_pin - 8, Mode::OUTPUT_50MHZ);
            w.set_cnf(gpio_pin - 8, Cnf::AF_OPEN_DRAIN_OUT);
        });
    } else {
        gpio_port.cfglr().modify(|w| {
            w.set_mode(gpio_pin, Mode::OUTPUT_50MHZ);
            w.set_cnf(gpio_pin, Cnf::AF_OPEN_DRAIN_OUT);
        });
    }

    // Configure USART for half-duplex mode at 1 Mbaud
    // Baud rate = APB2_CLK / (16 * USARTDIV) = 144 MHz / (16 * 9) = 1 Mbaud
    usart.brr().write_value(pac::usart::regs::Brr(9));

    usart.ctlr1().modify(|w| {
        w.set_m(false); // 8 data bits
        w.set_te(true); // Transmitter enable
        w.set_re(true); // Receiver enable
        w.set_idleie(true); // IDLE line interrupt enable
    });

    usart.ctlr3().modify(|w| {
        w.set_hdsel(true); // Half-duplex mode
        w.set_dmat(true); // DMA enable for transmitter
        w.set_dmar(true); // DMA enable for receiver
    });

    // Configure DMA TX channel (one-shot mode)
    pac::DMA1.ch(dma_tx_channel - 1).cr().write(|w| {
        w.set_dir(Dir::FROMMEMORY); // Memory to peripheral
        w.set_circ(false); // One-shot mode (not circular)
        w.set_pinc(false); // Peripheral address fixed
        w.set_minc(true); // Memory address increment
        w.set_psize(Size::BITS8); // Peripheral data size: 8 bits
        w.set_msize(Size::BITS8); // Memory data size: 8 bits
        w.set_pl(Pl::MEDIUM); // Priority level: medium
        w.set_tcie(true); // Transfer complete interrupt enable
        w.set_teie(true); // Transfer error interrupt enable
        w.set_en(false); // Channel disabled initially
    });

    pac::DMA1
        .ch(dma_tx_channel - 1)
        .par()
        .write_value(usart.datar().as_ptr() as u32);

    // Configure DMA RX channel (circular buffer mode)
    pac::DMA1.ch(dma_rx_channel - 1).cr().write(|w| {
        w.set_dir(Dir::FROMPERIPHERAL); // Peripheral to memory
        w.set_circ(true); // Circular buffer mode
        w.set_pinc(false); // Peripheral address fixed
        w.set_minc(true); // Memory address increment
        w.set_psize(Size::BITS8); // Peripheral data size: 8 bits
        w.set_msize(Size::BITS8); // Memory data size: 8 bits
        w.set_pl(Pl::MEDIUM); // Priority level: medium
        w.set_htie(true); // Half-transfer interrupt enable
        w.set_tcie(true); // Transfer complete interrupt enable
        w.set_teie(true); // Transfer error interrupt enable
        w.set_en(false); // Channel disabled initially
    });

    pac::DMA1
        .ch(dma_rx_channel - 1)
        .par()
        .write_value(usart.datar().as_ptr() as u32);

    // Set RX DMA memory address and buffer size
    pac::DMA1
        .ch(dma_rx_channel - 1)
        .mar()
        .write_value(rx_dma_buffer.as_ptr() as u32);
    pac::DMA1
        .ch(dma_rx_channel - 1)
        .ndtr()
        .write(|w| w.set_ndt(rx_dma_buffer.len() as u16));

    // Enable RX DMA channel
    pac::DMA1
        .ch(dma_rx_channel - 1)
        .cr()
        .modify(|w| w.set_en(true));

    compiler_fence(Ordering::SeqCst);

    // Enable USART
    usart.ctlr1().modify(|w| w.set_ue(true));
}

/// USART RX handle for async operations
pub struct UsartRx {
    consumer: fring::Consumer<'static, u8, USART_RX_CAPACITY>,
    waker: &'static AtomicWaker,
}

impl UsartRx {
    /// Async read from USART RX buffer
    /// Waits until data is available, then returns a fring::Region with all available data
    pub async fn read(
        &mut self,
        target_len: usize,
    ) -> fring::Region<'_, fring::Consumer<'static, u8, USART_RX_CAPACITY>, u8> {
        poll_fn(|cx| {
            self.waker.register(cx.waker());

            compiler_fence(Ordering::SeqCst);

            // Check if data is available
            if self.consumer.data_size() > 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        // Data is available, return the region
        self.consumer.read(target_len)
    }

    pub fn read_nb(
        &mut self,
        target_len: usize,
    ) -> fring::Region<'_, fring::Consumer<'static, u8, USART_RX_CAPACITY>, u8> {
        self.consumer.read(target_len)
    }

    /// Async read from USART RX buffer until a zero byte is encountered
    pub async fn read_until_zero(
        &mut self,
    ) -> fring::Region<'_, fring::Consumer<'static, u8, USART_RX_CAPACITY>, u8> {
        let region = self.read(usize::MAX).await;
        let mut null_index = region.len() - 1;
        for i in 0..region.len() {
            if region[i] == 0 {
                null_index = i;
                break;
            }
        }
        core::mem::forget(region);
        self.read_nb(null_index + 1)
    }
}

#[allow(dead_code)]
pub struct Hardware {
    pub leds: Leds,
    pub led_pwr: LedPwr,
    pub usarts_tx: [UsartTx; 4],
    pub usarts_rx: [UsartRx; 4],
}

impl Hardware {
    pub fn init() -> Self {
        // Configure system clock to 144 MHz from HSI
        // HSI = 8 MHz, PLL = HSI * 18 = 144 MHz
        // Enable HSI
        pac::RCC.ctlr().modify(|w| w.set_hsion(true));
        while !pac::RCC.ctlr().read().hsirdy() {}

        // Configure main PLL: HSI * 18 = 144 MHz
        pac::RCC.cfgr0().modify(|w| {
            w.set_pllsrc(false); // HSI as PLL source
            w.set_pllmul(PllMul::MUL18); // PLL multiplier x18
        });

        // Configure bus prescalers (all DIV1)
        pac::RCC.cfgr0().modify(|w| {
            w.set_hpre(Hpre::DIV1); // AHB prescaler = 1
            w.set_ppre1(Ppre::DIV1); // APB1 prescaler = 1
            w.set_ppre2(Ppre::DIV1); // APB2 prescaler = 1
        });

        // Enable PLL
        pac::RCC.ctlr().modify(|w| w.set_pllon(true));
        while !pac::RCC.ctlr().read().pllrdy() {}

        // Switch to PLL as system clock
        pac::RCC.cfgr0().modify(|w| w.set_sw(Sw::PLL));
        while pac::RCC.cfgr0().read().sws() != Sw::PLL {}

        // Set mtvec to point to the vector table
        // Use VectoredAddress mode for absolute addressing
        unsafe {
            qingke::register::mtvec::write(
                &_sflash as *const () as usize,
                qingke::register::mtvec::TrapMode::VectoredAddress,
            );
        }

        // Initialize embassy time driver with SysTick (HCLK = 144 MHz)
        critical_section::with(|cs| {
            TIME_DRIVER.init(cs, 144_000_000);
        });

        // Enable peripheral clocks
        pac::RCC.apb2pcenr().modify(|w| {
            //w.set_iopcen(true); // GPIOC clock
            w.set_iopaen(true); // GPIOA clock for USART1, USART2
            w.set_iopben(true); // GPIOB clock for SPI1, USART3, USART4
            w.set_afioen(true); // AFIO clock
            w.set_spi1en(true); // SPI1 clock
            w.set_usart1en(true); // USART1 clock
        });
        pac::RCC.apb1pcenr().modify(|w| {
            w.set_usart2en(true); // USART2 clock
            w.set_usart3en(true); // USART3 clock
            w.set_usart4en(true); // USART4 clock
        });
        pac::RCC.ahbpcenr().modify(|w| {
            w.set_dma1en(true); // DMA1 clock
        });

        unsafe {
            // Enable SDI print
            core::ptr::write_volatile(DEBUG_DATA0_ADDRESS, 0);
            riscv::asm::delay(100000);
        }

        // PB3 enables LED power when high
        // Configure as push-pull output, 50 MHz
        // Start with it HIGH (power disabled)
        pac::GPIOB.bshr().write(|w| w.set_bs(3, true));
        compiler_fence(Ordering::SeqCst);
        pac::GPIOB.cfglr().modify(|w| {
            w.set_mode(3, Mode::OUTPUT_50MHZ);
            w.set_cnf(3, Cnf::ANALOG_IN__PUSH_PULL_OUT);
        });

        // Configure SPI1 pins for WS2812 output
        // PB5: MOSI
        pac::GPIOB.cfglr().modify(|w| {
            w.set_mode(5, Mode::OUTPUT_50MHZ);
            w.set_cnf(5, Cnf::AF_OPEN_DRAIN_OUT);
        });
        // SPI1_RM=1 in order to use PB5
        pac::AFIO.pcfr1().modify(|w| {
            w.set_spi1_rm(true);
        });

        // APB2 = 144 MHz, desired SPI clock = 3.2 MHz
        // 144 MHz / 32 / 2 = 2.25 MHz (closest to 3.2 MHz)
        pac::SPI1.ctlr1().modify(|w| {
            w.set_cpha(false); // Clock phase: first edge
            w.set_cpol(false); // Clock polarity: low when idle
            w.set_mstr(true); // Master mode
            w.set_br(BaudRate::DIV_32); // Baud rate: APB2/64 = 2.25 MHz
            w.set_spe(false); // SPI disabled during configuration
            w.set_lsbfirst(false); // MSB first
            w.set_ssi(true); // Internal slave select high
            w.set_ssm(true); // Software slave management
            w.set_rxonly(false); // Full duplex
            w.set_dff(true); // 16-bit data frame format
            w.set_crcen(false); // CRC disabled
            w.set_bidimode(false); // 2-line unidirectional mode
        });

        // Configure interrupt nesting with 2 levels, 1 preemption bit (bit 7)
        // INTSYSCR register (CSR 0x804):
        //   bits[3:2] = PMTCFG = 0b01 (2 nested levels, bit7 is preemption bit)
        //   bit[1] = INESTEN = 1 (interrupt nesting enabled)
        //   bit[0] = HWSTKEN = 1 (hardware stack enabled)
        // Value: 0b0000_0111 = 0x7
        unsafe {
            core::arch::asm!("csrw 0x804, {0}", in(reg) 0x7_usize);
        }

        // Initialize all 4 USARTs in half-duplex mode at 1 Mbaud
        unsafe {
            // USART1: PA9, DMA1_CH4 (TX), DMA1_CH5 (RX)
            init_usart_halfduplex(
                pac::USART1,
                pac::GPIOA,
                9,
                4, // DMA1_CH4 for TX
                5, // DMA1_CH5 for RX
                &mut USART1_RX_DMA_BUFFER,
            );

            // USART2: PA2, DMA1_CH7 (TX), DMA1_CH6 (RX)
            init_usart_halfduplex(
                pac::USART2,
                pac::GPIOA,
                2,
                7, // DMA1_CH7 for TX
                6, // DMA1_CH6 for RX
                &mut USART2_RX_DMA_BUFFER,
            );

            // USART3: PB10, DMA1_CH2 (TX), DMA1_CH3 (RX)
            init_usart_halfduplex(
                pac::USART3,
                pac::GPIOB,
                10,
                2, // DMA1_CH2 for TX
                3, // DMA1_CH3 for RX
                &mut USART3_RX_DMA_BUFFER,
            );

            // USART4: PB0, DMA1_CH1 (TX), DMA1_CH8 (RX)
            init_usart_halfduplex(
                pac::USART4,
                pac::GPIOB,
                0,
                1, // DMA1_CH1 for TX
                8, // DMA1_CH8 for RX
                &mut USART4_RX_DMA_BUFFER,
            );
        }

        // Enable all interrupts in PFIC with appropriate priorities
        // For PMTCFG=0b01: bit7 is the preemption bit, 0x00 = high priority, 0x80 = low priority
        unsafe {
            // SPI1 has highest priority (can preempt all USART/DMA interrupts)
            qingke::pfic::enable_interrupt(pac::Interrupt::SPI1 as u8);
            qingke::pfic::set_priority(pac::Interrupt::SPI1 as u8, 0x00); // High preemption priority (bit7=0)

            // SysTick for embassy_time
            qingke::pfic::enable_interrupt(qingke_rt::CoreInterrupt::SysTick as u8);
            qingke::pfic::set_priority(qingke_rt::CoreInterrupt::SysTick as u8, 0xFF); // Lowest priority

            // USART1 and its DMA channels
            qingke::pfic::enable_interrupt(pac::Interrupt::USART1 as u8);
            qingke::pfic::set_priority(pac::Interrupt::USART1 as u8, 0x80); // Low preemption priority (bit7=1)
            qingke::pfic::enable_interrupt(pac::Interrupt::DMA1_CHANNEL4 as u8);
            qingke::pfic::set_priority(pac::Interrupt::DMA1_CHANNEL4 as u8, 0x80);
            qingke::pfic::enable_interrupt(pac::Interrupt::DMA1_CHANNEL5 as u8);
            qingke::pfic::set_priority(pac::Interrupt::DMA1_CHANNEL5 as u8, 0x80);

            // USART2 and its DMA channels
            qingke::pfic::enable_interrupt(pac::Interrupt::USART2 as u8);
            qingke::pfic::set_priority(pac::Interrupt::USART2 as u8, 0x80);
            qingke::pfic::enable_interrupt(pac::Interrupt::DMA1_CHANNEL7 as u8);
            qingke::pfic::set_priority(pac::Interrupt::DMA1_CHANNEL7 as u8, 0x80);
            qingke::pfic::enable_interrupt(pac::Interrupt::DMA1_CHANNEL6 as u8);
            qingke::pfic::set_priority(pac::Interrupt::DMA1_CHANNEL6 as u8, 0x80);

            // USART3 and its DMA channels
            qingke::pfic::enable_interrupt(pac::Interrupt::USART3 as u8);
            qingke::pfic::set_priority(pac::Interrupt::USART3 as u8, 0x80);
            qingke::pfic::enable_interrupt(pac::Interrupt::DMA1_CHANNEL2 as u8);
            qingke::pfic::set_priority(pac::Interrupt::DMA1_CHANNEL2 as u8, 0x80);
            qingke::pfic::enable_interrupt(pac::Interrupt::DMA1_CHANNEL3 as u8);
            qingke::pfic::set_priority(pac::Interrupt::DMA1_CHANNEL3 as u8, 0x80);

            // USART4 and its DMA channels
            qingke::pfic::enable_interrupt(pac::Interrupt::UART4 as u8);
            qingke::pfic::set_priority(pac::Interrupt::UART4 as u8, 0x80);
            qingke::pfic::enable_interrupt(pac::Interrupt::DMA1_CHANNEL1 as u8);
            qingke::pfic::set_priority(pac::Interrupt::DMA1_CHANNEL1 as u8, 0x80);
            qingke::pfic::enable_interrupt(pac::Interrupt::DMA1_CHANNEL8 as u8);
            qingke::pfic::set_priority(pac::Interrupt::DMA1_CHANNEL8 as u8, 0x80);
        }

        // Enable SEVONPEND so WFI wakes on pending interrupts even when globally disabled
        // From ch32-hal:
        // > The WCH QingKe RISC-V core deviates from standard RISC-V specification:
        // > - `WFI` instruction will not wake up from disabled interrupts
        // > - Either `WFITOWFE` or `SEVONPEND` must be enabled for proper wake-up behavior
        pac::PFIC.sctlr().modify(|w| w.set_sevonpend(true));

        compiler_fence(Ordering::SeqCst);

        // Enable SPI1
        pac::SPI1.ctlr1().modify(|w| w.set_spe(true));

        compiler_fence(Ordering::SeqCst);

        Hardware {
            leds: Leds {},
            led_pwr: LedPwr {},
            // Order: USART3, USART1, USART4, USART2
            usarts_tx: [
                UsartTx {
                    //usart: pac::USART3,
                    dma_ch: 2,
                    waker: &USART3_TX_WAKER,
                },
                UsartTx {
                    //usart: pac::USART1,
                    dma_ch: 4,
                    waker: &USART1_TX_WAKER,
                },
                UsartTx {
                    //usart: pac::USART4,
                    dma_ch: 1,
                    waker: &USART4_TX_WAKER,
                },
                UsartTx {
                    //usart: pac::USART2,
                    dma_ch: 7,
                    waker: &USART2_TX_WAKER,
                },
            ],
            usarts_rx: [
                UsartRx {
                    consumer: unsafe { USART3_RX_FRING.consumer() },
                    waker: &USART3_RX_WAKER,
                },
                UsartRx {
                    consumer: unsafe { USART1_RX_FRING.consumer() },
                    waker: &USART1_RX_WAKER,
                },
                UsartRx {
                    consumer: unsafe { USART4_RX_FRING.consumer() },
                    waker: &USART4_RX_WAKER,
                },
                UsartRx {
                    consumer: unsafe { USART2_RX_FRING.consumer() },
                    waker: &USART2_RX_WAKER,
                },
            ],
        }
    }
}

pub struct Leds {}

impl Leds {
    /// Send a new frame to the LEDs.
    /// The data is buffered and this function returns immediately.
    /// Returns Ok(()) if the data was buffered and will be sent.
    /// Returns Err(()) if there was insufficient space in the buffer for the given data.
    pub fn write_slice(&mut self, data: &[u8]) -> Result<(), ()> {
        {
            let producer = unsafe { &mut LED_FRAME_BUFFERS_PRODUCER };
            let mut region = producer.write(1);
            let buffer = region.first_mut().ok_or(())?;
            buffer.fill_from_slice(data);
        }

        compiler_fence(Ordering::SeqCst);

        // Enable interrupt. If TXE is set (peripheral ready), interrupt fires immediately.
        // If TXE is clear (transmission in progress), interrupt fires when TXE becomes set.
        let spi1 = pac::SPI1;
        spi1.ctlr2().modify(|w| w.set_txeie(true));

        Ok(())
    }
}

/// SysTick interrupt, for embassy time driver
#[qingke_rt::interrupt(core)]
fn SysTick() {
    TIME_DRIVER.on_interrupt();
}

/// USART1 interrupt handler (for IDLE line detection)
#[qingke_rt::interrupt]
fn USART1() {
    unsafe {
        handle_usart_rx_copy(
            pac::USART1,
            5,
            &USART1_RX_DMA_BUFFER,
            &mut USART1_RX_FRING_PRODUCER,
            &mut USART1_RX_LAST_POS,
            &USART1_RX_WAKER,
        );
    }
}

/// Generic USART TX DMA interrupt handler
#[inline]
fn handle_usart_tx_dma_interrupt(channel: usize, waker: &AtomicWaker) {
    let isr = pac::DMA1.isr().read();
    if isr.teif(channel - 1) {
        // Clear error flag
        pac::DMA1.ifcr().write(|w| w.set_teif(channel - 1, true));
        panic!("DMA1_CH{} transfer error", channel);
    }
    if isr.tcif(channel - 1) {
        // Disable transfer
        pac::DMA1.ch(channel - 1).cr().modify(|w| w.set_en(false));
        // Clear transfer complete flag
        pac::DMA1.ifcr().write(|w| w.set_tcif(channel - 1, true));
        compiler_fence(Ordering::SeqCst);
        waker.wake();
    }
}

/// Generic USART RX handler - handles data copy from DMA buffer to fring
/// Checks for overrun, clears DMA and USART IDLE flags if set, and copies new data
#[inline]
fn handle_usart_rx_copy(
    usart: pac::usart::Usart,
    channel: usize,
    dma_buffer: &'static [u8],
    producer: &mut fring::Producer<u8, USART_RX_CAPACITY>,
    last_pos: &mut usize,
    waker: &AtomicWaker,
) {
    // Check and clear USART IDLE flag
    let statr = usart.statr().read();
    if statr.idle() {
        // Clear IDLE flag by reading STATR then DATAR
        let _ = usart.datar().read().dr();
    }

    let isr = pac::DMA1.isr().read();

    if isr.teif(channel - 1) {
        // Clear error flag
        pac::DMA1.ifcr().write(|w| w.set_teif(channel - 1, true));
        panic!("DMA1_CH{} RX transfer error", channel);
    }

    let htif = isr.htif(channel - 1);
    let tcif = isr.tcif(channel - 1);

    // Clear DMA interrupt flags if they're set
    if htif {
        pac::DMA1.ifcr().write(|w| w.set_htif(channel - 1, true));
    }
    if tcif {
        pac::DMA1.ifcr().write(|w| w.set_tcif(channel - 1, true));
    }

    // Get current DMA position
    let ndtr = pac::DMA1.ch(channel - 1).ndtr().read().ndt() as usize;
    let buffer_size = dma_buffer.len();
    let current_pos = buffer_size - ndtr;

    // Check for overrun: both HT and TC set means we're too slow
    if htif && tcif {
        // Overrun: skip copying, just update position to current DMA position
        *last_pos = current_pos;
    }

    if current_pos == *last_pos {
        return;
    }

    if current_pos > *last_pos {
        // Copy 1 congiguous src region
        let _ = producer.write_slice(&dma_buffer[*last_pos..current_pos]);
    } else {
        // Wraparound--copy 2 congiguous src regions
        let _ = producer.write_slice(&dma_buffer[*last_pos..]);
        let _ = producer.write_slice(&dma_buffer[..current_pos]);
    }
    *last_pos = current_pos;

    compiler_fence(Ordering::SeqCst);
    waker.wake();
}

/// DMA1_CHANNEL4 interrupt handler (USART1 TX complete)
#[qingke_rt::interrupt]
fn DMA1_CHANNEL4() {
    handle_usart_tx_dma_interrupt(4, &USART1_TX_WAKER);
}

/// DMA1_CHANNEL5 interrupt handler (USART1 RX half-transfer and transfer complete)
#[qingke_rt::interrupt]
fn DMA1_CHANNEL5() {
    unsafe {
        handle_usart_rx_copy(
            pac::USART1,
            5,
            &USART1_RX_DMA_BUFFER,
            &mut USART1_RX_FRING_PRODUCER,
            &mut USART1_RX_LAST_POS,
            &USART1_RX_WAKER,
        );
    }
}

/// USART2 interrupt handler (for IDLE line detection)
#[qingke_rt::interrupt]
fn USART2() {
    unsafe {
        handle_usart_rx_copy(
            pac::USART2,
            6,
            &USART2_RX_DMA_BUFFER,
            &mut USART2_RX_FRING_PRODUCER,
            &mut USART2_RX_LAST_POS,
            &USART2_RX_WAKER,
        );
    }
}

/// DMA1_CHANNEL7 interrupt handler (USART2 TX complete)
#[qingke_rt::interrupt]
fn DMA1_CHANNEL7() {
    handle_usart_tx_dma_interrupt(7, &USART2_TX_WAKER);
}

/// DMA1_CHANNEL6 interrupt handler (USART2 RX half-transfer and transfer complete)
#[qingke_rt::interrupt]
fn DMA1_CHANNEL6() {
    unsafe {
        handle_usart_rx_copy(
            pac::USART2,
            6,
            &USART2_RX_DMA_BUFFER,
            &mut USART2_RX_FRING_PRODUCER,
            &mut USART2_RX_LAST_POS,
            &USART2_RX_WAKER,
        );
    }
}

/// USART3 interrupt handler (for IDLE line detection)
#[qingke_rt::interrupt]
fn USART3() {
    unsafe {
        handle_usart_rx_copy(
            pac::USART3,
            3,
            &USART3_RX_DMA_BUFFER,
            &mut USART3_RX_FRING_PRODUCER,
            &mut USART3_RX_LAST_POS,
            &USART3_RX_WAKER,
        );
    }
}

/// DMA1_CHANNEL2 interrupt handler (USART3 TX complete)
#[qingke_rt::interrupt]
fn DMA1_CHANNEL2() {
    handle_usart_tx_dma_interrupt(2, &USART3_TX_WAKER);
}

/// DMA1_CHANNEL3 interrupt handler (USART3 RX half-transfer and transfer complete)
#[qingke_rt::interrupt]
fn DMA1_CHANNEL3() {
    unsafe {
        handle_usart_rx_copy(
            pac::USART3,
            3,
            &USART3_RX_DMA_BUFFER,
            &mut USART3_RX_FRING_PRODUCER,
            &mut USART3_RX_LAST_POS,
            &USART3_RX_WAKER,
        );
    }
}

/// USART4 interrupt handler (for IDLE line detection)
#[qingke_rt::interrupt]
fn UART4() {
    unsafe {
        handle_usart_rx_copy(
            pac::USART4,
            8,
            &USART4_RX_DMA_BUFFER,
            &mut USART4_RX_FRING_PRODUCER,
            &mut USART4_RX_LAST_POS,
            &USART4_RX_WAKER,
        );
    }
}

/// DMA1_CHANNEL1 interrupt handler (UART4 TX complete)
#[qingke_rt::interrupt]
fn DMA1_CHANNEL1() {
    handle_usart_tx_dma_interrupt(1, &USART4_TX_WAKER);
}

/// DMA1_CHANNEL8 interrupt handler (UART4 RX half-transfer and transfer complete)
#[qingke_rt::interrupt]
fn DMA1_CHANNEL8() {
    unsafe {
        handle_usart_rx_copy(
            pac::USART4,
            8,
            &USART4_RX_DMA_BUFFER,
            &mut USART4_RX_FRING_PRODUCER,
            &mut USART4_RX_LAST_POS,
            &USART4_RX_WAKER,
        );
    }
}

/// SPI1 interrupt handler
/// Drains the ring buffer until empty, then disables itself
#[qingke_rt::interrupt]
fn SPI1() {
    // Check if TXE (transmit buffer empty) flag is set
    if !pac::SPI1.statr().read().txe() {
        return;
    }

    loop {
        // See if we have an in-progress frame and that frame has another 4 bits to send
        if let Some(led_tx) = unsafe { LED_ACTIVE_TX.as_mut() } {
            if let Some(word) = led_tx
                .region
                .first_mut()
                .unwrap()
                .next_4bits(&mut led_tx.nibble_index)
            {
                pac::SPI1.datar().write(|w| w.set_datar(word));
                return;
            }

            // See if we are sending a reset pulse
            if led_tx.reset_pulse_count < 20 {
                pac::SPI1.datar().write(|w| w.set_datar(0x0000));
                led_tx.reset_pulse_count += 1;
                return;
            } else {
                unsafe {
                    LED_ACTIVE_TX = None;
                }
            }
        }
        compiler_fence(Ordering::SeqCst);

        // We don't have an in-progress frame, so see if there is one available.
        let consumer = unsafe { &mut LED_FRAME_BUFFERS_CONSUMER };
        let next_region = consumer.read(1);
        if next_region.len() > 0 {
            unsafe {
                LED_ACTIVE_TX = Some(LedTx {
                    region: next_region,
                    nibble_index: 0,
                    reset_pulse_count: 0,
                });
            }
            // Loop--try to send data
        } else {
            // Buffer empty - disable interrupt until next push
            pac::SPI1.ctlr2().modify(|w| w.set_txeie(false));
            return;
        }
    }
}

pub struct LedPwr {}

impl LedPwr {
    pub fn set_pwr(&mut self, on: bool) {
        if on {
            pac::GPIOB.bshr().write(|w| w.set_br(3, true));
        } else {
            pac::GPIOB.bshr().write(|w| w.set_bs(3, true));
        }
    }
}

/// Branch to bootloader or application firmware
unsafe fn branch(addr: *const ()) -> ! {
    critical_section::with(|_cs| {
        // Reset all hardware peripherals using RCC reset registers
        pac::RCC
            .apb2prstr()
            .write_value(pac::rcc::regs::Apb2prstr(0xFFFFFFFF));
        pac::RCC
            .apb1prstr()
            .write_value(pac::rcc::regs::Apb1prstr(0xFFFFFFFF));
        pac::RCC
            .ahbrstr()
            .write_value(pac::rcc::regs::Ahbrstr(0xFFFFFFFF));
        compiler_fence(Ordering::SeqCst);
        pac::RCC
            .apb2prstr()
            .write_value(pac::rcc::regs::Apb2prstr(0x00000000));
        pac::RCC
            .apb1prstr()
            .write_value(pac::rcc::regs::Apb1prstr(0x00000000));
        pac::RCC
            .ahbrstr()
            .write_value(pac::rcc::regs::Ahbrstr(0x00000000));

        // Reset clock to default state (HSI only, no PLL)
        // Switch to HSI before disabling PLL
        pac::RCC.cfgr0().modify(|w| w.set_sw(Sw::HSI));
        while pac::RCC.cfgr0().read().sws() != Sw::HSI {}
        pac::RCC.ctlr().modify(|w| w.set_pllon(false));

        // Reset bus prescalers to default (DIV1)
        pac::RCC.cfgr0().modify(|w| {
            w.set_hpre(Hpre::DIV1);
            w.set_ppre1(Ppre::DIV1);
            w.set_ppre2(Ppre::DIV1);
        });

        // Disable all peripheral clocks
        pac::RCC
            .apb2pcenr()
            .write_value(pac::rcc::regs::Apb2pcenr(0));
        pac::RCC
            .apb1pcenr()
            .write_value(pac::rcc::regs::Apb1pcenr(0));
        pac::RCC.ahbpcenr().write_value(pac::rcc::regs::Ahbpcenr(0));

        compiler_fence(Ordering::SeqCst);

        // Clear all interrupt enable and pending flags via PFIC
        // Disable all interrupts in PFIC (using IRER registers)
        const PFIC_IRER0: *mut u32 = 0xE000E180 as *mut u32;
        for offset in 0..4 {
            unsafe {
                core::ptr::write_volatile(PFIC_IRER0.offset(offset), 0xFFFFFFFF);
            }
        }

        // Clear all pending interrupts in PFIC (using IPRR registers)
        const PFIC_IPRR0: *mut u32 = 0xE000E280 as *mut u32;
        for offset in 0..4 {
            unsafe {
                core::ptr::write_volatile(PFIC_IPRR0.offset(offset), 0xFFFFFFFF);
            }
        }
    });

    compiler_fence(Ordering::SeqCst);

    // Branch to the app
    unsafe {
        core::arch::asm!(
            "jr {0}",
            in(reg) addr,
            options(noreturn)
        );
    }
}

/// Branch to application firmware.
/// Safety: Must not be called from inside an interrupt.
pub unsafe fn branch_to_app() -> ! {
    unsafe {
        branch(&_sapp);
    }
}

/// Branch to bootloader.
/// Safety: Must not be called from inside an interrupt.
pub unsafe fn branch_to_bootloader() -> ! {
    unsafe {
        branch(&_sbootloader);
    }
}
