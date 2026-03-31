/// Linux-specific process statistics collection.

#[cfg(target_os = "linux")]
pub fn clock_ticks_per_second() -> u64 {
    static TICKS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TICKS.get_or_init(|| {
        std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(100)
    })
}

#[cfg(not(target_os = "linux"))]
pub fn clock_ticks_per_second() -> u64 {
    100
}

/// Read RSS (bytes) and cumulative CPU time (ms) from /proc/{pid}.
/// Returns (rss_bytes, user_ms, system_ms).
pub fn procfs_stats(pid: u32) -> (u64, u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let rss = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
            .unwrap_or(0);

        let (user_ms, sys_ms) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                let after_comm = s.rfind(')')?.checked_add(2)?;
                let fields: Vec<&str> = s[after_comm..].split_whitespace().collect();
                let tps = clock_ticks_per_second();
                // user time is at field index 11 (zero-based after comm)
                // system time is at field index 12
                let u = fields.get(11)?.parse::<u64>().ok()? * 1000 / tps;
                let sy = fields.get(12)?.parse::<u64>().ok()? * 1000 / tps;
                Some((u, sy))
            })
            .unwrap_or((0, 0));

        (rss, user_ms, sys_ms)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        (0, 0, 0)
    }
}
