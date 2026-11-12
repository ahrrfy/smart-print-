use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Memory RSS metric snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct MemorySnapshot {
    pub resident_bytes: usize,
    pub peak_working_set_bytes: usize,
}

impl MemorySnapshot {
    pub fn capture_current() -> Self {
        #[cfg(windows)]
        {
            use std::mem::size_of;

            #[repr(C)]
            #[allow(non_snake_case)]
            struct PROCESS_MEMORY_COUNTERS {
                cb: u32,
                PageFaultCount: u32,
                PeakWorkingSetSize: usize,
                WorkingSetSize: usize,
                QuotaPeakPagedPoolUsage: usize,
                QuotaPagedPoolUsage: usize,
                QuotaPeakNonPagedPoolUsage: usize,
                QuotaNonPagedPoolUsage: usize,
                PagefileUsage: usize,
                PeakPagefileUsage: usize,
            }

            unsafe extern "system" {
                fn GetCurrentProcess() -> isize;
                fn K32GetProcessMemoryInfo(
                    process: isize,
                    counters: *mut PROCESS_MEMORY_COUNTERS,
                    cb: u32,
                ) -> i32;
            }

            let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
            counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;

            let success = unsafe {
                K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb)
            };

            if success != 0 {
                Self {
                    resident_bytes: counters.WorkingSetSize,
                    peak_working_set_bytes: counters.PeakWorkingSetSize,
                }
            } else {
                Self::default()
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
                let parts: Vec<&str> = statm.split_whitespace().collect();
                if parts.len() >= 2 {
                    let resident_pages: usize = parts[1].parse().unwrap_or(0);
                    let page_size = 4096;
                    return Self {
                        resident_bytes: resident_pages * page_size,
                        peak_working_set_bytes: resident_pages * page_size,
                    };
                }
            }
            Self::default()
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Self::default()
        }
    }
}

/// Automated background memory probe tracking RSS throughout a workload.
pub struct MemoryProbe {
    pub initial: MemorySnapshot,
    peak_rss_bytes: Arc<AtomicUsize>,
    stop_signal: Arc<AtomicBool>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
}

impl MemoryProbe {
    /// Starts background memory probe sampling at 25ms intervals.
    pub fn start() -> Self {
        let initial = MemorySnapshot::capture_current();
        let peak_rss_bytes = Arc::new(AtomicUsize::new(initial.resident_bytes));
        let stop_signal = Arc::new(AtomicBool::new(false));

        let peak_clone = Arc::clone(&peak_rss_bytes);
        let stop_clone = Arc::clone(&stop_signal);

        let worker_handle = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                let current = MemorySnapshot::capture_current();
                let prev_peak = peak_clone.load(Ordering::Relaxed);
                if current.resident_bytes > prev_peak {
                    peak_clone.store(current.resident_bytes, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        });

        Self {
            initial,
            peak_rss_bytes,
            stop_signal,
            worker_handle: Some(worker_handle),
        }
    }

    /// Stops sampling and returns (initial_bytes, peak_bytes, delta_bytes).
    pub fn stop(mut self) -> (usize, usize, usize) {
        self.stop_signal.store(true, Ordering::Relaxed);
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
        let peak = self.peak_rss_bytes.load(Ordering::Relaxed);
        let delta = peak.saturating_sub(self.initial.resident_bytes);
        (self.initial.resident_bytes, peak, delta)
    }
}

/// Fluidity statistics of the UI main thread.
#[derive(Debug, Clone)]
pub struct FluidityReport {
    pub total_frames: usize,
    pub dropped_frames: usize, // duration > target frame budget (e.g. 16.6ms)
    pub stutter_frames: usize, // duration > 33.3ms
    pub freeze_events: usize,  // duration > 100ms
    pub min_frame_duration_ms: f64,
    pub max_frame_duration_ms: f64,
    pub avg_frame_duration_ms: f64,
    pub p95_frame_duration_ms: f64,
    pub p99_frame_duration_ms: f64,
}

/// High-resolution frame duration sampler.
pub struct UiFluidityMonitor {
    pub target_frame_budget: Duration,
    frame_durations: Vec<Duration>,
    current_frame_start: Option<Instant>,
}

impl UiFluidityMonitor {
    pub fn new(target_fps: f64) -> Self {
        let budget_secs = 1.0 / target_fps;
        Self {
            target_frame_budget: Duration::from_secs_f64(budget_secs),
            frame_durations: Vec::with_capacity(1024),
            current_frame_start: None,
        }
    }

    pub fn begin_frame(&mut self) {
        self.current_frame_start = Some(Instant::now());
    }

    pub fn end_frame(&mut self) {
        if let Some(start) = self.current_frame_start.take() {
            self.frame_durations.push(start.elapsed());
        }
    }

    pub fn record_frame_duration(&mut self, duration: Duration) {
        self.frame_durations.push(duration);
    }

    pub fn finish(mut self) -> FluidityReport {
        if self.frame_durations.is_empty() {
            return FluidityReport {
                total_frames: 0,
                dropped_frames: 0,
                stutter_frames: 0,
                freeze_events: 0,
                min_frame_duration_ms: 0.0,
                max_frame_duration_ms: 0.0,
                avg_frame_duration_ms: 0.0,
                p95_frame_duration_ms: 0.0,
                p99_frame_duration_ms: 0.0,
            };
        }

        self.frame_durations.sort();

        let total_frames = self.frame_durations.len();
        let stutter_limit = Duration::from_millis(33);
        let freeze_limit = Duration::from_millis(100);

        let dropped = self
            .frame_durations
            .iter()
            .filter(|&&d| d > self.target_frame_budget)
            .count();
        let stutters = self
            .frame_durations
            .iter()
            .filter(|&&d| d > stutter_limit)
            .count();
        let freezes = self
            .frame_durations
            .iter()
            .filter(|&&d| d > freeze_limit)
            .count();

        let sum_ms: f64 = self
            .frame_durations
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .sum();
        let avg_ms = sum_ms / (total_frames as f64);
        let min_ms = self
            .frame_durations
            .first()
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        let max_ms = self
            .frame_durations
            .last()
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);

        let p95_idx = ((total_frames as f64) * 0.95) as usize;
        let p99_idx = ((total_frames as f64) * 0.99) as usize;

        let p95_ms = self
            .frame_durations
            .get(p95_idx.min(total_frames - 1))
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        let p99_ms = self
            .frame_durations
            .get(p99_idx.min(total_frames - 1))
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);

        FluidityReport {
            total_frames,
            dropped_frames: dropped,
            stutter_frames: stutters,
            freeze_events: freezes,
            min_frame_duration_ms: min_ms,
            max_frame_duration_ms: max_ms,
            avg_frame_duration_ms: avg_ms,
            p95_frame_duration_ms: p95_ms,
            p99_frame_duration_ms: p99_ms,
        }
    }
}

pub struct BenchmarkAssertions;

impl BenchmarkAssertions {
    /// Asserts that peak memory increase during multi-printer dispatch is bounded below max_mb.
    pub fn assert_peak_memory_delta(delta_bytes: usize, max_mb: f64) {
        let delta_mb = (delta_bytes as f64) / (1024.0 * 1024.0);
        assert!(
            delta_mb <= max_mb,
            "Peak RSS growth exceeded budget: {:.2} MB > allowed {:.2} MB (Zero-copy shared memory violation)",
            delta_mb, max_mb
        );
    }

    /// Asserts 60 FPS UI main thread fluidity (0 freezes, max frame under budget).
    pub fn assert_ui_fluidity_60fps(report: &FluidityReport) {
        assert_eq!(
            report.freeze_events, 0,
            "UI main thread experienced {} freeze event(s) (>100ms)",
            report.freeze_events
        );
        assert_eq!(
            report.stutter_frames, 0,
            "UI main thread experienced {} stutter frame(s) (>33ms)",
            report.stutter_frames
        );
        assert!(
            report.max_frame_duration_ms < 16.67,
            "Max frame duration exceeded 60 FPS budget: {:.2} ms > 16.67 ms",
            report.max_frame_duration_ms
        );
    }
}
