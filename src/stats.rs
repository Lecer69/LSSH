use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

/// A single snapshot of remote system usage.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// CPU usage in percent (needs two samples; `None` on the first poll).
    pub cpu: Option<f64>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub disk_used: Option<u64>,
    pub disk_total: Option<u64>,
    /// Receive/transmit rate in bytes per second.
    pub rx_rate: Option<f64>,
    pub tx_rate: Option<f64>,
    pub load: Option<(f64, f64, f64)>,
    pub uptime: Option<u64>,
    pub os: Option<String>,
    pub cores: Option<u32>,
}

/// Keeps the previous sample so rates can be computed on the next poll.
#[derive(Debug, Default, Clone)]
pub struct Collector {
    prev_cpu: Option<(f64, f64)>,
    prev_net: Option<(u64, u64, Instant)>,
}

impl Collector {
    pub fn new() -> Self {
        Collector::default()
    }
}

/// Marker-separated shell snippet run on the remote host.
///
/// It is sent as a single argument to the remote shell, so it is safe to pass
/// through the local process (no local shell interpretation).
pub const REMOTE_CMD: &str = concat!(
    "echo __LSSH_CPU__; cat /proc/stat; ",
    "echo __LSSH_MEM__; cat /proc/meminfo; ",
    "echo __LSSH_NET__; cat /proc/net/dev; ",
    "echo __LSSH_DISK__; df -P -B1 / 2>/dev/null | tail -n 1; ",
    "echo __LSSH_LOAD__; cat /proc/loadavg 2>/dev/null; ",
    "echo __LSSH_UP__; cat /proc/uptime 2>/dev/null; ",
    "echo __LSSH_OS__; uname -srm 2>/dev/null; ",
    "echo __LSSH_CORES__; (nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1); ",
    "echo __LSSH_END__"
);

/// Run the stats snippet over the existing ControlMaster connection.
///
/// Uses `BatchMode=yes` so it can never block waiting for a password: if the
/// multiplexed connection is not ready it simply fails and is retried later.
pub fn collect(target: &str, control_path: &Path) -> Result<String> {
    let output = Command::new("ssh")
        .arg("-o")
        .arg(format!("ControlPath={}", control_path.display()))
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg(target)
        .arg(REMOTE_CMD)
        .output()
        .context("failed to run ssh for stats")?;

    if !output.status.success() {
        bail!("stats ssh exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn marker(line: &str) -> Option<&'static str> {
    Some(match line {
        "__LSSH_CPU__" => "cpu",
        "__LSSH_MEM__" => "mem",
        "__LSSH_NET__" => "net",
        "__LSSH_DISK__" => "disk",
        "__LSSH_LOAD__" => "load",
        "__LSSH_UP__" => "up",
        "__LSSH_OS__" => "os",
        "__LSSH_CORES__" => "cores",
        "__LSSH_END__" => "end",
        _ => return None,
    })
}

/// Parse the marker-separated output and blend it with previous samples.
pub fn parse(raw: &str, collector: &mut Collector) -> Stats {
    let now = Instant::now();
    let mut stats = Stats::default();
    let mut section = "";
    let mut buf: Vec<&str> = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(name) = marker(trimmed) {
            flush(section, &buf, collector, &mut stats, now);
            buf.clear();
            if name == "end" {
                return stats;
            }
            section = name;
            continue;
        }
        buf.push(line);
    }
    flush(section, &buf, collector, &mut stats, now);
    stats
}

fn flush(
    section: &str,
    lines: &[&str],
    collector: &mut Collector,
    stats: &mut Stats,
    now: Instant,
) {
    match section {
        "cpu" => parse_cpu(first_line(lines), collector, stats),
        "mem" => parse_mem(lines, stats),
        "net" => parse_net(lines, collector, stats, now),
        "disk" => parse_disk(first_line(lines), stats),
        "load" => parse_load(first_line(lines), stats),
        "up" => parse_uptime(first_line(lines), stats),
        "os" => {
            let os = first_line(lines).trim();
            if !os.is_empty() {
                stats.os = Some(os.to_string());
            }
        }
        "cores" => {
            stats.cores = lines.iter().find_map(|l| l.trim().parse::<u32>().ok());
        }
        _ => {}
    }
}

/// First non-empty line of a section, else the empty string.
fn first_line<'a>(lines: &[&'a str]) -> &'a str {
    lines
        .iter()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn parse_cpu(line: &str, collector: &mut Collector, stats: &mut Stats) {
    let mut parts = line.split_whitespace();
    if parts.next() != Some("cpu") {
        return;
    }
    let nums: Vec<f64> = parts.filter_map(|s| s.parse().ok()).collect();
    if nums.len() < 4 {
        return;
    }
    let total: f64 = nums.iter().sum();
    let idle = nums.get(3).copied().unwrap_or(0.0) + nums.get(4).copied().unwrap_or(0.0);

    if let Some((prev_total, prev_idle)) = collector.prev_cpu {
        let dt = total - prev_total;
        let di = idle - prev_idle;
        if dt > 0.0 {
            stats.cpu = Some((((dt - di) / dt) * 100.0).clamp(0.0, 100.0));
        }
    }
    collector.prev_cpu = Some((total, idle));
}

fn parse_mem(lines: &[&str], stats: &mut Stats) {
    let (mut total, mut available, mut free) = (None, None, None);
    let (mut buffers, mut cached) = (0u64, 0u64);

    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(kb) = value
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        match key.trim() {
            "MemTotal" => total = Some(kb),
            "MemAvailable" => available = Some(kb),
            "MemFree" => free = Some(kb),
            "Buffers" => buffers = kb,
            "Cached" => cached = kb,
            _ => {}
        }
    }

    if let Some(total) = total {
        let avail = available.or_else(|| free.map(|f| f + buffers + cached));
        if let Some(avail) = avail {
            stats.mem_total = Some(total.saturating_mul(1024));
            stats.mem_used = Some(total.saturating_sub(avail).saturating_mul(1024));
        }
    }
}

fn parse_disk(line: &str, stats: &mut Stats) {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 3 {
        return;
    }
    if let (Ok(total), Ok(used)) = (fields[1].parse::<u64>(), fields[2].parse::<u64>()) {
        stats.disk_total = Some(total);
        stats.disk_used = Some(used);
    }
}

fn parse_load(line: &str, stats: &mut Stats) {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() >= 3
        && let (Ok(a), Ok(b), Ok(c)) = (
            fields[0].parse::<f64>(),
            fields[1].parse::<f64>(),
            fields[2].parse::<f64>(),
        )
    {
        stats.load = Some((a, b, c));
    }
}

fn parse_uptime(line: &str, stats: &mut Stats) {
    if let Some(secs) = line
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
    {
        stats.uptime = Some(secs.max(0.0) as u64);
    }
}

fn parse_net(lines: &[&str], collector: &mut Collector, stats: &mut Stats, now: Instant) {
    let mut rx_total = 0u64;
    let mut tx_total = 0u64;

    for line in lines {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        let iface = iface.trim();
        if iface.is_empty() || iface == "lo" {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 16 {
            continue;
        }
        rx_total = rx_total.saturating_add(fields[0].parse::<u64>().unwrap_or(0));
        tx_total = tx_total.saturating_add(fields[8].parse::<u64>().unwrap_or(0));
    }

    if let Some((prev_rx, prev_tx, prev_at)) = collector.prev_net {
        let dt = now.duration_since(prev_at).as_secs_f64();
        if dt > 0.0 {
            stats.rx_rate = Some(rx_total.saturating_sub(prev_rx) as f64 / dt);
            stats.tx_rate = Some(tx_total.saturating_sub(prev_tx) as f64 / dt);
        }
    }
    collector.prev_net = Some((rx_total, tx_total, now));
}

/// Short human-readable byte size, e.g. `512B`, `1.5K`, `2.3M`, `1.1T`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else if value >= 100.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// Rate as `<size>/s`, e.g. `1.5M/s`.
pub fn human_rate(bytes_per_sec: f64) -> String {
    human_bytes(bytes_per_sec.max(0.0) as u64) + "/s"
}

/// Uptime as `5d 3h`, `3h 12m` or `12m`.
pub fn human_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_1: &str = "\
__LSSH_CPU__
cpu  100 0 50 800 50 0 0 0 0 0
__LSSH_MEM__
MemTotal:       16000000 kB
MemFree:         2000000 kB
MemAvailable:    8000000 kB
Buffers:          500000 kB
Cached:          3000000 kB
__LSSH_NET__
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000 10 0 0 0 0 0 0 1000 10 0 0 0 0 0 0
  eth0: 5000000 5000 0 0 0 0 0 0 2000000 2000 0 0 0 0 0 0
__LSSH_DISK__
/dev/sda1 100000000000 40000000000 60000000000 40% /
__LSSH_LOAD__
0.52 0.58 0.59 1/234 5678
__LSSH_UP__
123456.78 987654.32
__LSSH_OS__
Linux 6.1.0-arch1 x86_64
__LSSH_CORES__
8
__LSSH_END__
";

    #[test]
    fn parses_first_sample() {
        let mut c = Collector::new();
        let s = parse(SAMPLE_1, &mut c);

        assert_eq!(s.cpu, None, "first sample has no delta yet");
        assert_eq!(s.mem_total, Some(16_000_000 * 1024));
        assert_eq!(s.mem_used, Some(8_000_000 * 1024));
        assert_eq!(s.disk_total, Some(100_000_000_000));
        assert_eq!(s.disk_used, Some(40_000_000_000));
        assert_eq!(s.load, Some((0.52, 0.58, 0.59)));
        assert_eq!(s.uptime, Some(123456));
        assert_eq!(s.os.as_deref(), Some("Linux 6.1.0-arch1 x86_64"));
        assert_eq!(s.cores, Some(8));
        assert_eq!(s.rx_rate, None, "first sample has no net delta yet");
    }

    #[test]
    fn computes_cpu_delta_on_second_sample() {
        let mut c = Collector::new();
        let _ = parse(SAMPLE_1, &mut c);

        let second = SAMPLE_1.replace(
            "cpu  100 0 50 800 50 0 0 0 0 0",
            "cpu  150 0 75 1600 75 0 0 0 0 0",
        );
        let s = parse(&second, &mut c);

        // total delta = 900, idle delta = 825 -> busy 75/900 = 8.333%
        let cpu = s.cpu.expect("cpu delta");
        assert!((cpu - 8.333).abs() < 0.01, "got {cpu}");
    }

    #[test]
    fn computes_net_delta_on_second_sample() {
        let mut c = Collector::new();
        let _ = parse(SAMPLE_1, &mut c);
        let second = SAMPLE_1.replace("eth0: 5000000", "eth0: 6000000");
        let s = parse(&second, &mut c);
        // Rate depends on wall clock, but a delta must now be present.
        assert!(s.rx_rate.is_some());
        assert!(s.tx_rate.is_some());
        assert!(s.rx_rate.unwrap().is_finite());
    }

    #[test]
    fn network_totals_do_not_double_count_interfaces() {
        let mut c = Collector::new();
        let _ = parse(SAMPLE_1, &mut c);
        let _ = parse(SAMPLE_1, &mut c); // same totals -> zero rate
        let s = parse(SAMPLE_1, &mut c);
        assert_eq!(s.rx_rate, Some(0.0));
        assert_eq!(s.tx_rate, Some(0.0));
    }

    #[test]
    fn fallback_memory_without_memavailable() {
        let raw = "\
__LSSH_MEM__
MemTotal:       1000 kB
MemFree:         100 kB
Buffers:          10 kB
Cached:           20 kB
__LSSH_END__
";
        let mut c = Collector::new();
        let s = parse(raw, &mut c);
        assert_eq!(s.mem_total, Some(1000 * 1024));
        assert_eq!(s.mem_used, Some((1000 - 130) * 1024));
    }

    #[test]
    fn malformed_input_does_not_panic() {
        let mut c = Collector::new();
        let s = parse("garbage\n__LSSH_CPU__\nnot cpu\n__LSSH_END__", &mut c);
        assert_eq!(s, Stats::default());
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1536), "1.5K");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0M");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0G");
    }

    #[test]
    fn formats_uptime() {
        assert_eq!(human_uptime(30), "0m");
        assert_eq!(human_uptime(3 * 3600 + 12 * 60), "3h 12m");
        assert_eq!(human_uptime(5 * 86400 + 3 * 3600), "5d 3h");
    }
}
