use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use time::OffsetDateTime;

use crate::{DATE_DAY, DATE_MONTH, DATE_YEAR, TIME_HOURS, TIME_MINUTES, ping_main_thread};

pub fn start(wake_fd: OwnedFd) {
    let _ = thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            println!("[Polling Thread] Started");
            loop {
                // 1. Get current time
                if let Ok(now) = OffsetDateTime::now_local() {
                    let current_hour = now.hour();
                    let current_minute = now.minute();
                    let current_day = now.day();
                    let current_month = u8::from(now.month());
                    // Get the last two digits of the year (e.g., 2026 -> 26)
                    let current_year = (now.year() % 100) as u8;

                    let mut changed = false;
                    if TIME_MINUTES.load(Ordering::Acquire) != current_minute {
                        TIME_MINUTES.store(current_minute, Ordering::Release);
                        TIME_HOURS.store(current_hour, Ordering::Release);
                        changed = true;
                    }
                    if DATE_DAY.load(Ordering::Acquire) != current_day {
                        DATE_DAY.store(current_day, Ordering::Release);
                        DATE_MONTH.store(current_month, Ordering::Release);
                        DATE_YEAR.store(current_year, Ordering::Release);
                        changed = true;
                    }

                    // 2. Read battery (Placeholder logic)

                    // Only wake up the main thread if the minute, date, or battery actually changed
                    if changed {
                        ping_main_thread(&wake_fd);
                    }
                }

                // Sleep until roughly the start of the next second to keep the clock accurate
                thread::sleep(Duration::from_millis(1000));
            }
        });
}
