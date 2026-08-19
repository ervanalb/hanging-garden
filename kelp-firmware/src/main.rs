#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::sync::atomic::{Ordering, compiler_fence};

use ch32_metapac as pac;
use pac::gpio::vals::{Cnf, Mode};
use pac::rcc::vals::{Hpre, PllMul, Ppre, Sw};
use pac::spi::vals::BaudRate;
use panic_halt as _;

const LED_FRAME_BUFFER_MAX_DATA_COUNT: usize = 1024;

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

struct Hardware {
    leds: Leds,
    led_pwr: LedPwr,
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

        // Enable peripheral clocks
        pac::RCC.apb2pcenr().modify(|w| {
            //w.set_iopcen(true); // GPIOC clock
            w.set_iopben(true); // GPIOB clock for SPI1
            w.set_afioen(true); // AFIO clock
            w.set_spi1en(true); // SPI1 clock
        });

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

        // Enable SPI1 interrupt in PFIC
        unsafe {
            qingke::pfic::enable_interrupt(pac::Interrupt::SPI1 as u8);
        }

        compiler_fence(Ordering::SeqCst);

        // Enable SPI1
        pac::SPI1.ctlr1().modify(|w| w.set_spe(true));

        compiler_fence(Ordering::SeqCst);

        Hardware {
            leds: Leds {},
            led_pwr: LedPwr {},
        }
    }
}

struct Leds {}

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
            }
        }

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
            unsafe {
                LED_ACTIVE_TX = None;
            }
            // Buffer empty - disable interrupt until next push
            pac::SPI1.ctlr2().modify(|w| w.set_txeie(false));
            return;
        }
    }
}

struct LedPwr {}

impl LedPwr {
    pub fn set_pwr(&mut self, on: bool) {
        if on {
            pac::GPIOB.bshr().write(|w| w.set_br(3, true));
        } else {
            pac::GPIOB.bshr().write(|w| w.set_bs(3, true));
        }
    }
}

#[qingke_rt::entry]
fn main() -> ! {
    let Hardware {
        mut leds,
        mut led_pwr,
    } = Hardware::init();

    led_pwr.set_pwr(true);

    loop {
        let _ = leds.write_slice(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF]);
        riscv::asm::delay(7_200_000); // ~0.5s at 144 MHz
        let _ = leds.write_slice(&[0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00]);
        riscv::asm::delay(7_200_000);
        let _ = leds.write_slice(&[0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00]);
        riscv::asm::delay(7_200_000);
    }

    /*
    // Blink loop
    loop {
        // Toggle PC13
        pac::GPIOC.bshr().write(|w| w.set_bs(13, true));
        riscv::asm::delay(7_200_000); // ~0.5s at 144 MHz

        pac::GPIOC.bshr().write(|w| w.set_br(13, true));
        riscv::asm::delay(7_200_000); // ~0.5s at 144 MHz
    }
    */
}
