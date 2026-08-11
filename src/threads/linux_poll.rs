use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use time::OffsetDateTime;

use crate::{
    BATTERY_MASK, DATE_MASK, TIME_MASK, pack_battery, pack_date, pack_time, ping_main_thread,
};

pub fn start(wake_fd: OwnedFd) {
    let _ = thread::Builder::new().spawn(move || {
        println!("[Polling Thread] Started");
        let mut tick_counter: u8 = 0;
        let mut last_minute: u8 = 255;
        let mut last_day: u8 = 255;

        loop {
            let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
            let current_minute = now.minute();
            let mut changed = false;

            if current_minute != last_minute {
                last_minute = current_minute;
                let new_time_mask = pack_time(now.hour(), current_minute);
                if TIME_MASK.load(Ordering::Relaxed) != new_time_mask {
                    TIME_MASK.store(new_time_mask, Ordering::Relaxed);
                    changed = true;
                }

                let current_day = now.day();
                if current_day != last_day {
                    last_day = current_day;
                    let current_month = u8::from(now.month());
                    let current_year = (now.year() % 100) as u8;
                    let new_date_mask = pack_date(current_day, current_month, current_year);
                    if DATE_MASK.load(Ordering::Relaxed) != new_date_mask {
                        DATE_MASK.store(new_date_mask, Ordering::Relaxed);
                        changed = true;
                    }
                }
            }

            // 2. Read battery every 30 ticks, but skip entirely if BATTERY_MASK is 0 (No Battery)
            if tick_counter.is_multiple_of(30)
                && BATTERY_MASK.load(Ordering::Relaxed) != 0
                && update_battery_state()
            {
                changed = true;
            }

            // Only wake up the main thread if the time, date, or battery actually changed
            if changed {
                ping_main_thread(&wake_fd);
            }

            tick_counter = tick_counter.wrapping_add(1);
            // Sleep until roughly the start of the next second to keep the clock accurate
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn read_sysfs_with<T>(path: &str, f: impl FnOnce(&str) -> Option<T>) -> Option<T> {
    let mut buf = [0u8; 24];
    let mut file = File::open(path).ok()?;
    let len = file.read(&mut buf).ok()?;
    f(std::str::from_utf8(&buf[..len]).ok()?.trim_end())
}

fn read_sysfs_num<T: std::str::FromStr>(path: &str) -> Option<T> {
    read_sysfs_with(path, |s| s.parse().ok())
}

fn update_battery_state() -> bool {
    let capacity: u8 = read_sysfs_num("/sys/class/power_supply/BAT0/capacity").unwrap_or(100);

    let state: u8 = read_sysfs_with("/sys/class/power_supply/BAT0/status", |s| {
        Some(match s {
            "Discharging" => 1,
            "Charging" => 2,
            "Full" => 3,
            _ => 0,
        })
    })
    .unwrap_or(0);

    // Calculate estimate
    let mut total_minutes = 0;
    if state == 1 || state == 2 {
        let current_now: u64 = read_sysfs_num("/sys/class/power_supply/BAT0/current_now")
            .or_else(|| read_sysfs_num("/sys/class/power_supply/BAT0/power_now"))
            .unwrap_or(0);

        if current_now > 0 {
            let charge_now: u64 = read_sysfs_num("/sys/class/power_supply/BAT0/charge_now")
                .or_else(|| read_sysfs_num("/sys/class/power_supply/BAT0/energy_now"))
                .unwrap_or(0);

            if state == 1 {
                let hours = charge_now as f64 / current_now as f64;
                total_minutes = (hours * 60.0) as u16;
            } else if state == 2 {
                let charge_full: u64 = read_sysfs_num("/sys/class/power_supply/BAT0/charge_full")
                    .or_else(|| read_sysfs_num("/sys/class/power_supply/BAT0/energy_full"))
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
