#![no_std]
#![no_main]

use core::sync::atomic::{Ordering, compiler_fence};

use ch32_metapac as pac;
use pac::gpio::vals::{Cnf, Mode};
use pac::rcc::vals::{Hpre, PllMul, Ppre, Sw};
use pac::spi::vals::BaudRate;
use panic_halt as _;

const LED_BUFFER_COUNT: usize = 1024;

// Global ring buffer
static LED_BUFFER: fring::Buffer<u16, LED_BUFFER_COUNT> = fring::Buffer::new();

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
            w.set_cnf(5, Cnf::PULL_IN__AF_PUSH_PULL_OUT); // TODO: Use open-drain
        });
        // SPI1_RM=1 in order to use PB5
        pac::AFIO.pcfr1().modify(|w| {
            w.set_spi1_rm(true);
        });

        // APB2 = 144 MHz, desired SPI clock = 4 MHz
        // 144 MHz / 32 = 4.5 MHz (closest to 4 MHz)
        pac::SPI1.ctlr1().modify(|w| {
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

        // Enable SPI1 interrupt in PFIC
        unsafe {
            qingke::pfic::enable_interrupt(pac::Interrupt::SPI1 as u8);
        }

        compiler_fence(Ordering::SeqCst);

        // Enable SPI1
        pac::SPI1.ctlr1().modify(|w| w.set_spe(true));

        compiler_fence(Ordering::SeqCst);

        Hardware {
            leds: Leds {
                buffer: unsafe { LED_BUFFER.producer() },
            },
            led_pwr: LedPwr {},
        }
    }
}

struct Leds {
    buffer: fring::Producer<'static, u16, LED_BUFFER_COUNT>,
}

impl Leds {
    /// Send new data to the LEDs.
    /// The data is buffered and this function returns immediately.
    /// Returns Ok(()) if the data was buffered and will be sent.
    /// Returns Err(()) if there was insufficient space in the buffer for the given data.
    pub fn spi_push(&mut self, data: &[u16]) -> Result<(), ()> {
        if data.len() > self.buffer.empty_size() {
            return Err(());
        }

        let data2 = {
            let mut region1 = self.buffer.write(data.len());
            let data1 = &data[..region1.len()];
            region1.copy_from_slice(data1);
            &data[region1.len()..]
        };
        if !data2.is_empty() {
            // 2 writes should always be sufficient
            // if there is available space
            let mut region2 = self.buffer.write(data2.len());
            region2.copy_from_slice(data2);
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
    let mut buffer = unsafe { LED_BUFFER.consumer() };

    // Check if TXE (transmit buffer empty) flag is set
    if pac::SPI1.statr().read().txe() {
        // Try to get next word from buffer
        if let Some(&word) = buffer.read(1).first() {
            // More data to send
            pac::SPI1.datar().write(|w| w.set_datar(word));
            // Interrupt stays enabled, will fire again when TXE becomes set
        } else {
            // Buffer empty - disable interrupt until next push
            pac::SPI1.ctlr2().modify(|w| w.set_txeie(false));
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
        let _ = leds.spi_push(&[0x0000; 64]);
    }

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
