use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::{fs, thread};
use time::OffsetDateTime;

use crate::{
    BATTERY_MASK, DATE_MASK, TIME_MASK, pack_battery, pack_date, pack_time, ping_main_thread,
};

pub fn start(wake_fd: OwnedFd) {
    let _ = thread::Builder::new().spawn(move || {
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
                let new_time_mask = pack_time(current_hour, current_minute);
                if TIME_MASK.load(Ordering::Relaxed) != new_time_mask {
                    TIME_MASK.store(new_time_mask, Ordering::Relaxed);
                    changed = true;
                }

                let new_date_mask = pack_date(current_day, current_month, current_year);
                if DATE_MASK.load(Ordering::Relaxed) != new_date_mask {
                    DATE_MASK.store(new_date_mask, Ordering::Relaxed);
                    changed = true;
                }

                // 2. Read battery every 30 ticks, but skip entirely if BATTERY_MASK is 0 (No Battery)
                if tick_counter % 30 == 0 && BATTERY_MASK.load(Ordering::Relaxed) != 0 {
                    tick_counter = 0;
                    if update_battery_state() {
                        changed = true;
                    }
                }

                // Only wake up the main thread if the time, date, or battery actually changed
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

fn read_sysfs<T: std::str::FromStr>(path: &str) -> Option<T> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn update_battery_state() -> bool {
    let mut capacity: u8 = 100;
    let mut state: u8 = 0;

    // Read capacity
    if let Some(c) = read_sysfs("/sys/class/power_supply/BAT0/capacity") {
        capacity = c;
    }

    // Read status
    if let Ok(s_str) = fs::read_to_string("/sys/class/power_supply/BAT0/status") {
        state = match s_str.trim() {
            "Discharging" => 1,
            "Charging" => 2,
            "Full" => 3,
            _ => 0,
        };
    }

    // Calculate estimate
    let mut total_minutes = 0;
    if state == 1 || state == 2 {
        let current_now: u64 = read_sysfs("/sys/class/power_supply/BAT0/current_now")
            .or_else(|| read_sysfs("/sys/class/power_supply/BAT0/power_now"))
            .unwrap_or(0);

        if current_now > 0 {
            let charge_now: u64 = read_sysfs("/sys/class/power_supply/BAT0/charge_now")
                .or_else(|| read_sysfs("/sys/class/power_supply/BAT0/energy_now"))
                .unwrap_or(0);

            if state == 1 {
                let hours = charge_now as f64 / current_now as f64;
                total_minutes = (hours * 60.0) as u16;
            } else if state == 2 {
                let charge_full: u64 = read_sysfs("/sys/class/power_supply/BAT0/charge_full")
                    .or_else(|| read_sysfs("/sys/class/power_supply/BAT0/energy_full"))
                    .unwrap_or(charge_now);

                if charge_full > charge_now {
                    let diff = charge_full - charge_now;
                    let hours = diff as f64 / current_now as f64;
                    total_minutes = (hours * 60.0) as u16;
                }
            }
        }
    }

    let new_bat_mask = pack_battery(capacity, state, total_minutes);
    if BATTERY_MASK.load(Ordering::Relaxed) != new_bat_mask {
        BATTERY_MASK.store(new_bat_mask, Ordering::Relaxed);
        true
    } else {
        false
    }
}
