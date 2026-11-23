use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct Pressure {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_us: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HostSnapshot {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub memory_total: u64,
    pub memory_available: u64,
    pub disk_used: u64,
    pub disk_total: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TenantSnapshot {
    pub memory_bytes: Option<u64>,
    pub cpu_us: Option<u64>,
    pub pids: Option<u64>,
}

pub fn read_pressure(kind: &str) -> Option<Pressure> {
    let text = std::fs::read_to_string(format!("/proc/pressure/{kind}")).ok()?;
    text.lines().find_map(parse_pressure_line)
}

fn parse_pressure_line(line: &str) -> Option<Pressure> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "some" {
        return None;
    }
    let (mut avg10, mut avg60, mut avg300, mut total_us) = (None, None, None, None);
    let percentage = |value: &str| {
        let value = value.parse::<f64>().ok()?;
        (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
    };
    for field in parts {
        let (key, value) = field.split_once('=')?;
        match key {
            "avg10" => avg10 = Some(percentage(value)?),
            "avg60" => avg60 = Some(percentage(value)?),
            "avg300" => avg300 = Some(percentage(value)?),
            "total" => total_us = Some(value.parse().ok()?),
            _ => {}
        }
    }
    Some(Pressure {
        avg10: avg10?,
        avg60: avg60?,
        avg300: avg300?,
        total_us: total_us?,
    })
}

pub fn host_snapshot(mount: &Path) -> HostSnapshot {
    let mut snapshot = HostSnapshot::default();

    if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
        let mut fields = text.split_whitespace();
        snapshot.load1 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        snapshot.load5 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        snapshot.load15 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    }

    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                snapshot.memory_total = kib(rest);
            } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                snapshot.memory_available = kib(rest);
            }
        }
    }

    if let Some((used, total)) = disk_usage(mount) {
        snapshot.disk_used = used;
        snapshot.disk_total = total;
    }

    snapshot
}

fn kib(value: &str) -> u64 {
    value
        .split_whitespace()
        .next()
        .and_then(|number| number.parse::<u64>().ok())
        .map(|kilobytes| kilobytes * 1024)
        .unwrap_or(0)
}

pub fn user_slice(uid: u32) -> PathBuf {
    PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice"))
}

pub fn tenant_snapshot(uid: u32) -> TenantSnapshot {
    let slice = user_slice(uid);
    TenantSnapshot {
        memory_bytes: read_memory_current(&slice),
        cpu_us: read_cpu_usage_us(&slice),
        pids: read_pids_current(&slice),
    }
}

pub fn read_memory_current(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn read_cpu_usage_us(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path.join("cpu.stat")).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix("usage_usec ")
            .and_then(|value| value.trim().parse().ok())
    })
}

pub fn read_pids_current(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path.join("pids.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;

    let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::statvfs(raw.as_ptr(), &mut stats) };
    if result != 0 {
        return None;
    }
    let block = stats.f_frsize as u64;
    let total = stats.f_blocks as u64 * block;
    let available = stats.f_bavail as u64 * block;
    Some((total.saturating_sub(available), total))
}

#[cfg(test)]
mod tests {
    use super::parse_pressure_line;

    #[test]
    fn pressure_requires_complete_finite_percentages_not_fabricated_zeroes() {
        let pressure =
            parse_pressure_line("some avg10=1.25 avg60=2.50 avg300=3.75 total=1234567").unwrap();
        assert_eq!(pressure.avg10, 1.25);
        assert_eq!(pressure.avg60, 2.50);
        assert_eq!(pressure.avg300, 3.75);
        assert_eq!(pressure.total_us, 1234567);
        for invalid in [
            "some avg10=0",
            "some avg10=0 avg60=0 avg300=0",
            "some avg10=NaN avg60=0 avg300=0 total=0",
            "some avg10=inf avg60=0 avg300=0 total=0",
            "some avg10=-1 avg60=0 avg300=0 total=0",
            "some avg10=101 avg60=0 avg300=0 total=0",
            "some avg10=0 avg60=0 avg300=0 total=-1",
            "full avg10=0 avg60=0 avg300=0 total=0",
        ] {
            assert!(parse_pressure_line(invalid).is_none(), "{invalid}");
        }
    }
}
