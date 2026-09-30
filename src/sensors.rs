//! Laptop state read from sysfs/procfs: battery level and lid switch.

use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatteryState {
    pub percent: u8,
    pub charging: bool,
}

/// The first battery under `/sys/class/power_supply`, if any.
pub fn battery() -> Option<BatteryState> {
    battery_in(Path::new("/sys/class/power_supply"))
}

fn battery_in(root: &Path) -> Option<BatteryState> {
    let mut supplies: Vec<_> = fs::read_dir(root).ok()?.flatten().map(|entry| entry.path()).collect();
    supplies.sort();
    supplies.into_iter().find_map(|supply| {
        let read = |name: &str| fs::read_to_string(supply.join(name)).ok().map(|value| value.trim().to_owned());
        if read("type")? != "Battery" {
            return None;
        }
        let percent = read("capacity")?.parse::<u8>().ok()?.min(100);
        Some(BatteryState { percent, charging: read("status").as_deref() == Some("Charging") })
    })
}

/// Whether mains power is connected, or `None` when no mains supply is listed.
pub fn on_mains() -> Option<bool> {
    on_mains_in(Path::new("/sys/class/power_supply"))
}

fn on_mains_in(root: &Path) -> Option<bool> {
    let mut online = None;
    for supply in fs::read_dir(root).ok()?.flatten() {
        let read = |name: &str| fs::read_to_string(supply.path().join(name)).ok().map(|value| value.trim().to_owned());
        if read("type").as_deref() == Some("Mains") {
            let plugged = read("online").as_deref() == Some("1");
            online = Some(online.unwrap_or(false) || plugged);
        }
    }
    online
}

/// Whether the lid is closed, or `None` when the machine exposes no lid switch.
pub fn lid_closed() -> Option<bool> {
    fs::read_dir("/proc/acpi/button/lid").ok()?.flatten().find_map(|entry| {
        let state = fs::read_to_string(entry.path().join("state")).ok()?;
        Some(state.contains("closed"))
    })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn mains_is_online_when_any_adapter_is() {
        let root = tempdir().unwrap();
        assert_eq!(on_mains_in(root.path()), None);
        for (name, kind, online) in [("BAT0", "Battery", "1"), ("AC0", "Mains", "0"), ("ADP1", "Mains", "1")] {
            let supply = root.path().join(name);
            fs::create_dir_all(&supply).unwrap();
            fs::write(supply.join("type"), format!("{kind}\n")).unwrap();
            fs::write(supply.join("online"), format!("{online}\n")).unwrap();
        }
        assert_eq!(on_mains_in(root.path()), Some(true));
        fs::write(root.path().join("ADP1/online"), "0\n").unwrap();
        assert_eq!(on_mains_in(root.path()), Some(false));
    }

    #[test]
    fn reads_first_battery_and_skips_mains() {
        let root = tempdir().unwrap();
        let ac = root.path().join("AC0");
        let battery = root.path().join("BAT0");
        fs::create_dir_all(&ac).unwrap();
        fs::create_dir_all(&battery).unwrap();
        fs::write(ac.join("type"), "Mains\n").unwrap();
        fs::write(battery.join("type"), "Battery\n").unwrap();
        fs::write(battery.join("capacity"), "87\n").unwrap();
        fs::write(battery.join("status"), "Charging\n").unwrap();
        assert_eq!(battery_in(root.path()), Some(BatteryState { percent: 87, charging: true }));
    }
}
