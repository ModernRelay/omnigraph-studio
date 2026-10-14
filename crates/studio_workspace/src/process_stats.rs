//! Studio's own memory and CPU use, for the footer when `studio.show_performance` is on
//! (spec 002). Memory is what the system charges the app: on macOS the physical footprint, the
//! figure Activity Monitor shows, and on Linux the resident set. Other platforms report none.

use std::time::{Duration, Instant};

/// One reading of the process.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub at: Instant,
    /// Bytes now, and the most the process has held.
    pub memory: Option<(u64, u64)>,
    /// CPU time used so far, user and system.
    pub cpu_time: Option<Duration>,
}

impl Sample {
    pub fn now() -> Self {
        Self {
            at: Instant::now(),
            // The system updates the peak lazily, so while memory grows it can trail the
            // current figure; the peak is at least what is held now.
            memory: memory().map(|(now, peak)| (now, peak.max(now))),
            cpu_time: cpu_time(),
        }
    }

    /// CPU used since `earlier`, as a share of one core: 1.0 is one core busy throughout.
    pub fn cpu_since(&self, earlier: &Sample) -> Option<f32> {
        let used = self.cpu_time?.checked_sub(earlier.cpu_time?)?;
        let wall = self.at.checked_duration_since(earlier.at)?;
        (!wall.is_zero()).then(|| used.as_secs_f32() / wall.as_secs_f32())
    }
}

#[cfg(target_os = "macos")]
fn memory() -> Option<(u64, u64)> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `info` is a `rusage_info_v4`, the struct the V4 flavor fills, and lives for the
    // call.
    let status = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so it filled `info`.
    let info = unsafe { info.assume_init() };
    Some((info.ri_phys_footprint, info.ri_lifetime_max_phys_footprint))
}

#[cfg(target_os = "linux")]
fn memory() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kilobytes = |key: &str| -> Option<u64> {
        let line = status.lines().find(|line| line.starts_with(key))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok()
    };
    Some((kilobytes("VmRSS:")? * 1024, kilobytes("VmHWM:")? * 1024))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn memory() -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn cpu_time() -> Option<Duration> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a `rusage` that lives for the call.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so it filled `usage`.
    let usage = unsafe { usage.assume_init() };
    let time = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    Some(time(usage.ru_utime) + time(usage.ru_stime))
}

#[cfg(not(unix))]
fn cpu_time() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_process_reads_its_memory_and_cpu() {
        let before = Sample::now();
        let (memory, peak) = before.memory.expect("memory");
        assert!(memory > 1 << 20, "at least a megabyte: {memory}");
        assert!(peak >= memory);
        // Some work, so the CPU clock moves.
        let mut x = 0u64;
        for i in 0..20_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let after = Sample::now();
        let share = after.cpu_since(&before).expect("a share");
        assert!(share > 0.0 && share < 64.0, "{share}");
    }
}
