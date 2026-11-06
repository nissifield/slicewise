//! Read-only cgroupfs and /proc reads with injectable roots (design §5.3, §10).
//! This module never writes; systemd is the only writer.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
pub const PROC_ROOT: &str = "/proc";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuMax {
    pub quota_us: Option<u64>,
    pub period_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psi {
    pub some_avg10: f32,
    pub full_avg10: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SliceStats {
    pub memory_current: u64,
    pub inactive_file: u64,
    pub pids_current: u64,
    pub cpu_pressure: Option<Psi>,
    pub memory_pressure: Option<Psi>,
}

fn path(root: &Path, rel: &str) -> PathBuf {
    root.join(rel.trim_start_matches('/'))
}

pub fn read_trimmed(root: &Path, rel: &str) -> io::Result<String> {
    Ok(fs::read_to_string(path(root, rel))?.trim().to_string())
}

pub fn read_u64(root: &Path, rel: &str) -> io::Result<u64> {
    read_trimmed(root, rel)?
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// `max 100000` means unlimited quota; `150000 100000` is 150 % of one CPU.
pub fn parse_cpu_max(s: &str) -> Option<CpuMax> {
    let mut fields = s.split_whitespace();
    let quota = fields.next()?;
    let period_us = fields.next()?.parse().ok()?;
    let quota_us = match quota {
        "max" => None,
        q => Some(q.parse().ok()?),
    };
    Some(CpuMax {
        quota_us,
        period_us,
    })
}

/// `full` is absent from some pressure files (for example `cpu.pressure`).
pub fn parse_psi(s: &str) -> Option<Psi> {
    let mut some = None;
    let mut full = None;
    for line in s.lines() {
        let mut fields = line.split_whitespace();
        let Some(kind) = fields.next() else {
            continue;
        };
        let avg10 = fields
            .find_map(|f| f.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f32>().ok());
        match (kind, avg10) {
            ("some", Some(v)) => some = Some(v),
            ("full", Some(v)) => full = Some(v),
            _ => {}
        }
    }
    Some(Psi {
        some_avg10: some?,
        full_avg10: full,
    })
}

pub fn parse_memory_stat(s: &str) -> BTreeMap<&str, u64> {
    s.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?, fields.next()?.parse().ok()?))
        })
        .collect()
}

pub fn parse_meminfo_bytes(s: &str) -> BTreeMap<&str, u64> {
    s.lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let mut fields = rest.split_whitespace();
            let value: u64 = fields.next()?.parse().ok()?;
            let scale = if fields.next() == Some("kB") { 1024 } else { 1 };
            Some((key, value * scale))
        })
        .collect()
}

/// `0-7` or `0-3,6` to a count.
pub fn parse_online_cpus(s: &str) -> Option<u32> {
    let mut total = 0;
    for part in s.trim().split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let (first, last): (u32, u32) = (first.parse().ok()?, last.parse().ok()?);
        total += last.checked_sub(first)? + 1;
    }
    Some(total)
}

/// cgroup v2 line `0::/user.slice/…` from `/proc/<pid>/cgroup`.
pub fn parse_proc_cgroup(s: &str) -> Option<&str> {
    s.lines().find_map(|line| line.strip_prefix("0::"))
}

pub fn slice_stats(root: &Path, rel: &str) -> io::Result<SliceStats> {
    let memory_current = read_u64(root, &format!("{rel}/memory.current"))?;
    let stat = read_trimmed(root, &format!("{rel}/memory.stat"))?;
    let stat = parse_memory_stat(&stat);
    let pressure = |name: &str| {
        read_trimmed(root, &format!("{rel}/{name}"))
            .ok()
            .and_then(|s| parse_psi(&s))
    };
    Ok(SliceStats {
        memory_current,
        inactive_file: stat.get("inactive_file").copied().unwrap_or(0),
        pids_current: read_u64(root, &format!("{rel}/pids.current")).unwrap_or(0),
        cpu_pressure: pressure("cpu.pressure"),
        memory_pressure: pressure("memory.pressure"),
    })
}

pub fn mem_total_bytes(proc_root: &Path) -> io::Result<u64> {
    let text = fs::read_to_string(proc_root.join("meminfo"))?;
    parse_meminfo_bytes(&text)
        .get("MemTotal")
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MemTotal missing"))
}

pub fn mem_available_bytes(proc_root: &Path) -> io::Result<u64> {
    let text = fs::read_to_string(proc_root.join("meminfo"))?;
    parse_meminfo_bytes(&text)
        .get("MemAvailable")
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MemAvailable missing"))
}

pub fn online_cpus(path: &Path) -> io::Result<u32> {
    parse_online_cpus(&fs::read_to_string(path)?)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad online CPU list"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_max_parses_quota_and_period() {
        assert_eq!(
            parse_cpu_max("150000 100000\n"),
            Some(CpuMax {
                quota_us: Some(150_000),
                period_us: 100_000
            })
        );
        assert_eq!(
            parse_cpu_max("max 100000"),
            Some(CpuMax {
                quota_us: None,
                period_us: 100_000
            })
        );
        assert_eq!(parse_cpu_max(""), None);
    }

    #[test]
    fn psi_parses_some_and_optional_full() {
        let cpu = "some avg10=12.50 avg60=3.00 avg300=1.00 total=99\n";
        assert_eq!(
            parse_psi(cpu),
            Some(Psi {
                some_avg10: 12.5,
                full_avg10: None
            })
        );
        let memory = "some avg10=1.00 avg60=0.00 avg300=0.00 total=1\nfull avg10=0.50 avg60=0.00 avg300=0.00 total=1\n";
        assert_eq!(
            parse_psi(memory),
            Some(Psi {
                some_avg10: 1.0,
                full_avg10: Some(0.5)
            })
        );
    }

    #[test]
    fn memory_stat_parses_keys() {
        let stat = parse_memory_stat("anon 1294336\nfile 8192\ninactive_file 4096\n");
        assert_eq!(stat.get("anon"), Some(&1_294_336));
        assert_eq!(stat.get("inactive_file"), Some(&4096));
        assert_eq!(stat.get("missing"), None);
    }

    #[test]
    fn meminfo_scales_kb() {
        let info = parse_meminfo_bytes("MemTotal: 1000 kB\nMemAvailable: 250 kB\nHugePages: 4\n");
        assert_eq!(info.get("MemTotal"), Some(&1_024_000));
        assert_eq!(info.get("MemAvailable"), Some(&256_000));
        assert_eq!(info.get("HugePages"), Some(&4));
    }

    #[test]
    fn online_cpu_ranges() {
        assert_eq!(parse_online_cpus("0-7"), Some(8));
        assert_eq!(parse_online_cpus("0"), Some(1));
        assert_eq!(parse_online_cpus("0-3,6"), Some(5));
        assert_eq!(parse_online_cpus("7-0"), None);
    }

    #[test]
    fn proc_cgroup_uses_v2_line() {
        assert_eq!(
            parse_proc_cgroup("1:name=systemd:/old\n0::/user.slice/user@$UID.service\n"),
            Some("/user.slice/user@$UID.service")
        );
        assert_eq!(parse_proc_cgroup(""), None);
    }
}
