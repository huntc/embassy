#![no_std]
#![no_main]
#![feature(min_type_alias_impl_trait)]
#![feature(impl_trait_in_bindings)]
#![feature(type_alias_impl_trait)]
#![allow(incomplete_features)]

#[path = "../example_common.rs"]
mod example_common;

use defmt::panic;
use embassy::executor::Spawner;
use embassy::time::{Duration, Timer};
use embassy::util::{Forever, mpsc};
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::Peripherals;
use embedded_hal::digital::v2::OutputPin;

enum LedState {
    On,
    Off,
}

static CHANNEL: Forever<mpsc::Channel<LedState, 1>> = Forever::new();

#[embassy::task(pool_size = 1)]
async fn my_task(sender: mpsc::Sender<'static, LedState, 1>) {
    loop {
        let _ = sender.send(LedState::On).await;
        Timer::after(Duration::from_secs(1)).await;
        let _ = sender.send(LedState::Off).await;
        Timer::after(Duration::from_secs(1)).await;
    }
}

#[embassy::main]
async fn main(spawner: Spawner, p: Peripherals) {
    let mut led = Output::new(p.P0_13, Level::Low, OutputDrive::Standard);

    let channel = CHANNEL.put(mpsc::Channel::new());
    let (sender, mut receiver) = mpsc::split(channel);

    spawner.spawn(my_task(sender)).unwrap();

    loop {
        match receiver.recv().await {
            Some(LedState::On) => led.set_high().unwrap(),
            Some(LedState::Off) => led.set_low().unwrap(),
            _ => (),
        }
    }
}
