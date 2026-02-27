use std::fs;
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use time::OffsetDateTime;

use crate::{
    BATTERY_ESTIMATE_M, BATTERY_PERCENT, BATTERY_STATE, DATE_DAY, DATE_MONTH, DATE_YEAR,
    TIME_HOURS, TIME_MINUTES, ping_main_thread,
};

pub fn start(wake_fd: OwnedFd) {
    let _ = thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            println!("[Polling Thread] Started");
            let mut tick_counter = 0;
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

                    // 2. Read battery every 30 ticks, but skip entirely if BATTERY_STATE is 255
                    if tick_counter % 30 == 0 && BATTERY_STATE.load(Ordering::Acquire) != 255 {
                        tick_counter = 0;
                        if update_battery_state() {
                            changed = true;
                        }
                    }

                    // Only wake up the main thread if the minute, date, or battery actually changed
                    if changed {
                        ping_main_thread(&wake_fd);
                    }
                }

                tick_counter += 1;
                // Sleep until roughly the start of the next second to keep the clock accurate
                thread::sleep(Duration::from_secs(1));
            }
        });
}

fn update_battery_state() -> bool {
    let mut changed = false;

    // Helper to read sysfs values
    let read_sysfs = |path: &str| -> Option<String> {
        fs::read_to_string(path).ok().map(|s| s.trim().to_string())
    };

    let read_sysfs_u32 =
        |path: &str| -> Option<u32> { read_sysfs(path).and_then(|s| s.parse().ok()) };

    let capacity = read_sysfs_u32("/sys/class/power_supply/BAT0/capacity").unwrap_or(100) as u8;
    if BATTERY_PERCENT.load(Ordering::Acquire) != capacity {
        BATTERY_PERCENT.store(capacity, Ordering::Release);
        changed = true;
    }

    let status_str = read_sysfs("/sys/class/power_supply/BAT0/status").unwrap_or_default();
    let state = match status_str.as_str() {
        "Discharging" => 1,
        "Charging" => 2,
        "Full" => 3,
        _ => 0, // Unknown or Not charging
    };

    if BATTERY_STATE.load(Ordering::Acquire) != state {
        BATTERY_STATE.store(state, Ordering::Release);
        changed = true;
    }

    // Calculate estimate
    let mut total_minutes = 0;

    if state == 1 || state == 2 {
        // Discharging or Charging
        let current_now = read_sysfs_u32("/sys/class/power_supply/BAT0/current_now")
            .or_else(|| read_sysfs_u32("/sys/class/power_supply/BAT0/power_now"))
            .unwrap_or(0);

        if current_now > 0 {
            let charge_now = read_sysfs_u32("/sys/class/power_supply/BAT0/charge_now")
                .or_else(|| read_sysfs_u32("/sys/class/power_supply/BAT0/energy_now"))
                .unwrap_or(0);

            if state == 1 {
                // Discharging
                let hours = charge_now as f64 / current_now as f64;
                total_minutes = (hours * 60.0) as u16;
            } else if state == 2 {
                // Charging
                let charge_full = read_sysfs_u32("/sys/class/power_supply/BAT0/charge_full")
                    .or_else(|| read_sysfs_u32("/sys/class/power_supply/BAT0/energy_full"))
                    .unwrap_or(charge_now);

                if charge_full > charge_now {
                    let diff = charge_full - charge_now;
                    let hours = diff as f64 / current_now as f64;
                    total_minutes = (hours * 60.0) as u16;
                }
            }
        }
    }

    if BATTERY_ESTIMATE_M.load(Ordering::Acquire) != total_minutes {
        BATTERY_ESTIMATE_M.store(total_minutes, Ordering::Release);
        changed = true;
    }

    changed
}
