#[cfg(target_os = "hermit")]
use hermit as _;

use std::time::Duration;
use std::thread;

fn main() {
    println!("Hello, world!");
    loop {
        // Stall silently without printing so we can read the logs.
        thread::sleep(Duration::from_secs(1));
    }
}
