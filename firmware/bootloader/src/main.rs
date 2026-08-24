#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use hal::Hardware;

#[qingke_rt::entry]
fn main() -> ! {
    let _hw = Hardware::init();

    // TODO: Implement bootloader logic
    // - Check for firmware update
    // - Verify firmware integrity
    // - Jump to application

    loop {
        // Placeholder: infinite loop
    }
}
