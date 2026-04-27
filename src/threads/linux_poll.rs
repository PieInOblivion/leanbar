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
    let mut capacity: u8 = 100;
    let mut state: u8 = 0;

    // Read capacity
    if let Ok(c_str) = fs::read_to_string("/sys/class/power_supply/BAT0/capacity")
        && let Ok(c) = c_str.trim().parse::<u8>()
    {
        capacity = c;
    }
    if BATTERY_PERCENT.load(Ordering::Acquire) != capacity {
        BATTERY_PERCENT.store(capacity, Ordering::Release);
        changed = true;
    }

    // Read status
    if let Ok(s_str) = fs::read_to_string("/sys/class/power_supply/BAT0/status") {
        let s = match s_str.trim() {
            "Discharging" => 1,
            "Charging" => 2,
            "Full" => 3,
            _ => 0,
        };
        state = s;
    }
    if BATTERY_STATE.load(Ordering::Acquire) != state {
        BATTERY_STATE.store(state, Ordering::Release);
        changed = true;
    }

    // Calculate estimate
    let mut total_minutes = 0;
    if state == 1 || state == 2 {
        let mut current_now = 0;
        if let Ok(s) = fs::read_to_string("/sys/class/power_supply/BAT0/current_now")
            .or_else(|_| fs::read_to_string("/sys/class/power_supply/BAT0/power_now"))
        {
            current_now = s.trim().parse().unwrap_or(0);
        }

        if current_now > 0 {
            let mut charge_now = 0;
            if let Ok(s) = fs::read_to_string("/sys/class/power_supply/BAT0/charge_now")
                .or_else(|_| fs::read_to_string("/sys/class/power_supply/BAT0/energy_now"))
            {
                charge_now = s.trim().parse().unwrap_or(0);
            }

            if state == 1 {
                let hours = charge_now as f64 / current_now as f64;
                total_minutes = (hours * 60.0) as u16;
            } else if state == 2 {
                let mut charge_full = charge_now;
                if let Ok(s) = fs::read_to_string("/sys/class/power_supply/BAT0/charge_full")
                    .or_else(|_| fs::read_to_string("/sys/class/power_supply/BAT0/energy_full"))
                {
                    charge_full = s.trim().parse().unwrap_or(charge_now);
                }

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
