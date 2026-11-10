//! Native Linux metrics collector. Reads `/proc` and `/sys` directly.
//! All parsers take `&str` so they can be unit-tested with fixtures.

use serde::Serialize;
use std::{ffi::CString, fs, mem::MaybeUninit};

/// Cumulative CPU tick counters from the aggregate `cpu` line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuTimes {
    pub idle: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Memory {
    pub total: u64,
    pub free: u64,
    pub available: u64,
    pub buffers: u64,
    pub cached: u64,
    /// Real memory in use: total - available.
    pub used: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub swap_used: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct DiskUsage {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MountEntry {
    pub device: String,
    pub mount_point: String,
    pub fstype: String,
}

/// One timestamped point of the stored history (all values are percentages).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sample {
    pub ts: i64,
    pub cpu: f64,
    pub memory: f64,
    pub swap: f64,
    pub disk: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cpu_cores: usize,
    pub uptime_secs: f64,
    pub time: i64,
    pub processes_total: usize,
    pub processes_running: u64,
    pub load_avg: [f64; 3],
    pub memory: Memory,
    pub disk: DiskUsage,
}

// ---------------------------------------------------------------- parsers

pub fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest is already in user)
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    let total: u64 = v.iter().take(8).sum();
    Some(CpuTimes { idle, total })
}

/// Busy percentage between two readings.
pub fn cpu_percent(prev: CpuTimes, cur: CpuTimes) -> f64 {
    let dt = cur.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0.0;
    }
    let di = cur.idle.saturating_sub(prev.idle);
    (dt.saturating_sub(di) as f64 / dt as f64 * 100.0).clamp(0.0, 100.0)
}

pub fn parse_procs_running(stat: &str) -> u64 {
    stat.lines()
        .find_map(|l| l.strip_prefix("procs_running "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

pub fn parse_meminfo(s: &str) -> Memory {
    let get = |key: &str| -> u64 {
        s.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let total = get("MemTotal");
    let free = get("MemFree");
    let buffers = get("Buffers");
    let cached = get("Cached");
    let available = match get("MemAvailable") {
        0 => free + buffers + cached,
        a => a,
    };
    let swap_total = get("SwapTotal");
    let swap_free = get("SwapFree");
    Memory {
        total,
        free,
        available,
        buffers,
        cached,
        used: total.saturating_sub(available),
        swap_total,
        swap_free,
        swap_used: swap_total.saturating_sub(swap_free),
    }
}

pub fn parse_uptime(s: &str) -> f64 {
    s.split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

pub fn parse_loadavg(s: &str) -> [f64; 3] {
    let mut it = s.split_whitespace().map(|v| v.parse().unwrap_or(0.0));
    [
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
    ]
}

pub fn parse_os_release(s: &str) -> String {
    s.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "Linux".to_string())
}

/// Returns (model name, logical core count).
pub fn parse_cpuinfo(s: &str) -> (String, usize) {
    let cores = s.lines().filter(|l| l.starts_with("processor")).count();
    let model = s
        .lines()
        .find(|l| l.starts_with("model name") || l.starts_with("Hardware") || l.starts_with("Model"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| "Unknown".to_string());
    (model, cores.max(1))
}

/// Block-device backed mounts, de-duplicated by device (bind mounts, btrfs
/// subvolumes) and without loop/squashfs images.
pub fn parse_mounts(s: &str) -> Vec<MountEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in s.lines() {
        let mut f = line.split_whitespace();
        let (Some(device), Some(mp), Some(fstype)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if !device.starts_with("/dev/") || device.starts_with("/dev/loop") || fstype == "squashfs" {
            continue;
        }
        if seen.insert(device.to_string()) {
            out.push(MountEntry {
                device: device.to_string(),
                mount_point: mp.replace("\\040", " "),
                fstype: fstype.to_string(),
            });
        }
    }
    out
}

pub fn pct(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

pub fn make_sample(ts: i64, cpu: f64, mem: &Memory, disk: &DiskUsage) -> Sample {
    Sample {
        ts,
        cpu,
        memory: pct(mem.used, mem.total),
        swap: pct(mem.swap_used, mem.swap_total),
        disk: pct(disk.used, disk.total),
    }
}

// ---------------------------------------------------------------- readers

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

pub fn read_cpu_times() -> Option<CpuTimes> {
    parse_cpu_times(&read("/proc/stat"))
}

pub fn read_memory() -> Memory {
    parse_meminfo(&read("/proc/meminfo"))
}

/// (total, free_blocks, available_to_unprivileged) in bytes.
#[allow(clippy::unnecessary_cast)]
fn statvfs(path: &str) -> Option<(u64, u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut s = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c` is a valid NUL-terminated string and `s` is a valid out-pointer.
    let rc = unsafe { libc::statvfs(c.as_ptr(), s.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so the struct is initialised.
    let s = unsafe { s.assume_init() };
    let fr = s.f_frsize as u64;
    Some((s.f_blocks as u64 * fr, s.f_bfree as u64 * fr, s.f_bavail as u64 * fr))
}

pub fn read_disk() -> DiskUsage {
    let mut d = DiskUsage::default();
    let mut avail_sum = 0u64;
    for m in parse_mounts(&read("/proc/mounts")) {
        if let Some((total, bfree, bavail)) = statvfs(&m.mount_point) {
            d.total += total;
            d.used += total.saturating_sub(bfree);
            avail_sum += bavail;
        }
    }
    d.free = avail_sum;
    d
}

fn count_processes() -> usize {
    fs::read_dir("/proc")
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
                .count()
        })
        .unwrap_or(0)
}

pub fn read_system_info(now: i64) -> SystemInfo {
    let (cpu_model, cpu_cores) = parse_cpuinfo(&read("/proc/cpuinfo"));
    SystemInfo {
        hostname: read("/proc/sys/kernel/hostname").trim().to_string(),
        os: parse_os_release(&read("/etc/os-release")),
        kernel: read("/proc/sys/kernel/osrelease").trim().to_string(),
        cpu_model,
        cpu_cores,
        uptime_secs: parse_uptime(&read("/proc/uptime")),
        time: now,
        processes_total: count_processes(),
        processes_running: parse_procs_running(&read("/proc/stat")),
        load_avg: parse_loadavg(&read("/proc/loadavg")),
        memory: read_memory(),
        disk: read_disk(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 1 1 1\nprocs_running 3\n";
    const STAT_B: &str = "cpu  150 0 150 900 0 0 0 0 0 0\nprocs_running 1\n";

    #[test]
    fn cpu_delta_percent() {
        let a = parse_cpu_times(STAT_A).unwrap();
        let b = parse_cpu_times(STAT_B).unwrap();
        // delta total = 200, delta idle = 100 -> 50% busy
        assert_eq!(cpu_percent(a, b), 50.0);
        assert_eq!(cpu_percent(a, a), 0.0);
    }

    #[test]
    fn cpu_rejects_garbage() {
        assert!(parse_cpu_times("nothing here").is_none());
    }

    #[test]
    fn procs_running() {
        assert_eq!(parse_procs_running(STAT_A), 3);
        assert_eq!(parse_procs_running(""), 0);
    }

    #[test]
    fn meminfo() {
        let s = "MemTotal: 1000 kB\nMemFree: 200 kB\nMemAvailable: 600 kB\nBuffers: 50 kB\nCached: 150 kB\nSwapTotal: 400 kB\nSwapFree: 300 kB\n";
        let m = parse_meminfo(s);
        assert_eq!(m.total, 1000 * 1024);
        assert_eq!(m.used, 400 * 1024);
        assert_eq!(m.cached, 150 * 1024);
        assert_eq!(m.swap_used, 100 * 1024);
    }

    #[test]
    fn meminfo_without_available() {
        let m = parse_meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 100 kB\nCached: 100 kB\n");
        assert_eq!(m.available, 300 * 1024);
    }

    #[test]
    fn uptime_and_load() {
        assert_eq!(parse_uptime("12345.67 9999.00\n"), 12345.67);
        assert_eq!(parse_loadavg("0.10 0.20 0.30 1/234 5678\n"), [0.10, 0.20, 0.30]);
        assert_eq!(parse_loadavg(""), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn os_release() {
        assert_eq!(
            parse_os_release("NAME=x\nPRETTY_NAME=\"openSUSE Leap 16.0\"\n"),
            "openSUSE Leap 16.0"
        );
        assert_eq!(parse_os_release(""), "Linux");
    }

    #[test]
    fn cpuinfo() {
        let s = "processor : 0\nmodel name : Foo CPU\nprocessor : 1\nmodel name : Foo CPU\n";
        assert_eq!(parse_cpuinfo(s), ("Foo CPU".to_string(), 2));
    }

    #[test]
    fn mounts_filtered_and_deduped() {
        let s = "/dev/sda1 / ext4 rw 0 0\n/dev/sda1 /bind ext4 rw 0 0\nproc /proc proc rw 0 0\n/dev/loop3 /snap/x squashfs ro 0 0\n/dev/sdb1 /my\\040data xfs rw 0 0\n";
        let m = parse_mounts(s);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].mount_point, "/");
        assert_eq!(m[1].mount_point, "/my data");
    }

    #[test]
    fn percentages() {
        assert_eq!(pct(50, 200), 25.0);
        assert_eq!(pct(1, 0), 0.0);
    }

    #[test]
    fn live_proc_smoke() {
        // Runs on Linux CI: the real readers must return sane values.
        let info = read_system_info(0);
        assert!(info.memory.total > 0);
        assert!(info.cpu_cores >= 1);
        assert!(info.uptime_secs > 0.0);
    }
}
