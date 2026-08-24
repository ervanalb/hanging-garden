#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use embassy_time::Timer;
use hal::Hardware;

#[embassy_executor::task]
async fn main_task() -> ! {
    Timer::after_millis(500).await;
    // Safety: we are not in an interrupt
    unsafe {
        hal::branch_to_app();
    }
}

#[qingke_rt::entry]
fn main() -> ! {
    let _ = Hardware::init();

    // Create executor
    let executor = embassy_executor::Executor::new();
    let executor = unsafe {
        static mut EXECUTOR: core::mem::MaybeUninit<embassy_executor::Executor> =
            core::mem::MaybeUninit::uninit();
        EXECUTOR.write(executor)
    };

    executor.run(|spawner| {
        spawner.spawn(main_task().unwrap());
    });
}
