#![no_std]
#![no_main]

use core::sync::atomic::{AtomicUsize, Ordering, compiler_fence};

use ch32_metapac as pac;
use pac::gpio::vals::{Cnf, Mode};
use pac::rcc::vals::{Hpre, PllMul, Ppre, Sw};
use pac::spi::vals::BaudRate;
use panic_halt as _;

// Ring buffer configuration
const RING_BUFFER_SIZE: usize = 256;

/// Lock-free SPSC (Single Producer Single Consumer) ring buffer
/// Producer (main thread) writes to head, Consumer (interrupt) reads from tail
struct RingBuffer {
    buffer: [u16; RING_BUFFER_SIZE],
    head: AtomicUsize,  // Modified only by producer
    tail: AtomicUsize,  // Modified only by consumer
}

impl RingBuffer {
    const fn new() -> Self {
        Self {
            buffer: [0; RING_BUFFER_SIZE],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Push a value (called only from main thread - single producer)
    fn push(&self, value: u16) -> Result<(), ()> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire); // Acquire to sync with consumer
        let next_head = (head + 1) % RING_BUFFER_SIZE;

        if next_head == tail {
            return Err(()); // Buffer full
        }

        // SAFETY: Single producer, we have exclusive write access to head position
        unsafe {
            let buf_ptr = self.buffer.as_ptr() as *mut u16;
            buf_ptr.add(head).write(value);
        }

        // Release store to make data visible to consumer
        self.head.store(next_head, Ordering::Release);
        Ok(())
    }

    /// Pop a value (called only from interrupt - single consumer)
    fn pop(&self) -> Option<u16> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire); // Acquire to sync with producer

        if head == tail {
            return None; // Buffer empty
        }

        // SAFETY: Single consumer, we have exclusive read access to tail position
        let value = unsafe {
            let buf_ptr = self.buffer.as_ptr();
            buf_ptr.add(tail).read()
        };

        let next_tail = (tail + 1) % RING_BUFFER_SIZE;
        // Release store to make space visible to producer
        self.tail.store(next_tail, Ordering::Release);

        Some(value)
    }
}

// Global ring buffer
static TX_BUFFER: RingBuffer = RingBuffer::new();

/// Push data to SPI transmit buffer and start transmission if needed
///
/// This is lock-free! We use the peripheral's TXE flag and TXEIE bit as synchronization:
/// - Enabling TXEIE when TXE is set causes the interrupt to fire
/// - The interrupt drains the buffer and disables TXEIE when empty
/// - Next push re-enables TXEIE, restarting the cycle
pub fn spi_push(data: u16) -> Result<(), ()> {
    // Add data to buffer (lock-free SPSC)
    TX_BUFFER.push(data)?;

    // Enable interrupt. If TXE is set (peripheral ready), interrupt fires immediately.
    // If TXE is clear (transmission in progress), interrupt fires when TXE becomes set.
    let spi1 = pac::SPI1;
    spi1.ctlr2().modify(|w| w.set_txeie(true));

    Ok(())
}

/// SPI1 interrupt handler
/// Drains the ring buffer until empty, then disables itself
#[qingke_rt::interrupt]
fn SPI1() {
    let spi1 = pac::SPI1;

    // Check if TXE (transmit buffer empty) flag is set
    if spi1.statr().read().txe() {
        // Try to get next word from buffer
        if let Some(word) = TX_BUFFER.pop() {
            // More data to send
            spi1.datar().write(|w| w.set_datar(word));
            // Interrupt stays enabled, will fire again when TXE becomes set
        } else {
            // Buffer empty - disable interrupt until next push
            spi1.ctlr2().modify(|w| w.set_txeie(false));
        }
    }
}

#[qingke_rt::entry]
fn main() -> ! {
    // Configure system clock to 144 MHz from HSI
    // HSI = 8 MHz, PLL = HSI * 18 = 144 MHz
    let rcc = pac::RCC;

    // Enable HSI
    rcc.ctlr().modify(|w| w.set_hsion(true));
    while !rcc.ctlr().read().hsirdy() {}

    // Configure main PLL: HSI * 18 = 144 MHz
    rcc.cfgr0().modify(|w| {
        w.set_pllsrc(false); // HSI as PLL source
        w.set_pllmul(PllMul::MUL18); // PLL multiplier x18
    });

    // Configure bus prescalers (all DIV1)
    rcc.cfgr0().modify(|w| {
        w.set_hpre(Hpre::DIV1); // AHB prescaler = 1
        w.set_ppre1(Ppre::DIV1); // APB1 prescaler = 1
        w.set_ppre2(Ppre::DIV1); // APB2 prescaler = 1
    });

    // Enable PLL
    rcc.ctlr().modify(|w| w.set_pllon(true));
    while !rcc.ctlr().read().pllrdy() {}

    // Switch to PLL as system clock
    rcc.cfgr0().modify(|w| w.set_sw(Sw::PLL));
    while rcc.cfgr0().read().sws() != Sw::PLL {}

    // Enable peripheral clocks
    rcc.apb2pcenr().modify(|w| {
        w.set_iopcen(true); // GPIOC clock
        w.set_iopben(true); // GPIOB clock for SPI1
        w.set_afioen(true); // AFIO clock
        w.set_spi1en(true); // SPI1 clock
    });

    // Configure PC13 as push-pull output, 50 MHz
    let gpioc = pac::GPIOC;
    gpioc.cfghr().modify(|w| {
        w.set_mode(13 - 8, Mode::OUTPUT_50MHZ);
        w.set_cnf(13 - 8, Cnf::ANALOG_IN__PUSH_PULL_OUT);
    });

    // Configure SPI1 pins
    let gpiob = pac::GPIOB;
    // PB3: SCK - Alternate function push-pull
    // PB5: MOSI - Alternate function push-pull
    gpiob.cfglr().modify(|w| {
        w.set_mode(3, Mode::OUTPUT_50MHZ);
        w.set_cnf(3, Cnf::PULL_IN__AF_PUSH_PULL_OUT); // SCK
        w.set_mode(5, Mode::OUTPUT_50MHZ);
        w.set_cnf(5, Cnf::PULL_IN__AF_PUSH_PULL_OUT); // MOSI
    });

    // Configure SPI1
    // APB2 = 144 MHz, desired SPI clock = 4 MHz
    // 144 MHz / 32 = 4.5 MHz (closest to 4 MHz)
    let spi1 = pac::SPI1;
    spi1.ctlr1().modify(|w| {
        w.set_cpha(false); // Clock phase: first edge
        w.set_cpol(false); // Clock polarity: low when idle
        w.set_mstr(true); // Master mode
        w.set_br(BaudRate::DIV_32); // Baud rate: APB2/32 = ~4.5 MHz
        w.set_spe(false); // SPI disabled during configuration
        w.set_lsbfirst(false); // MSB first
        w.set_ssi(true); // Internal slave select high
        w.set_ssm(true); // Software slave management
        w.set_rxonly(false); // Full duplex
        w.set_dff(true); // 16-bit data frame format
        w.set_crcen(false); // CRC disabled
        w.set_bidimode(false); // 2-line unidirectional mode
    });

    compiler_fence(Ordering::SeqCst);

    // Enable SPI1
    spi1.ctlr1().modify(|w| w.set_spe(true));

    compiler_fence(Ordering::SeqCst);

    // Enable SPI1 interrupt in PFIC
    unsafe {
        qingke::pfic::enable_interrupt(pac::Interrupt::SPI1 as u8);
    }

    // Test: Send some data
    for i in 0..10 {
        let _ = spi_push(0xAA00 | i);
    }

    // Blink loop
    loop {
        // Toggle PC13
        pac::GPIOC.bshr().write(|w| w.set_bs(13, true));
        riscv::asm::delay(7_200_000); // ~0.5s at 144 MHz

        pac::GPIOC.bshr().write(|w| w.set_br(13, true));
        riscv::asm::delay(7_200_000); // ~0.5s at 144 MHz
    }
}
