//! Empirical Challenger Suite for Feature 20 (Spooler Disk Space Pre-Check)
//! and Feature 21 (Two-Tier Concurrency Governor).
//!
//! Evaluates:
//! 1. Strict permit caps (CPU <= min(CPUs, 4), Spool <= 2) under heavy contention.
//! 2. Permit clamp safety (release cannot exceed maximum permit capacity).
//! 3. Panic safety & RAII unwinding clean release across multiple concurrent workers.
//! 4. Cooperative cancellation latency (< 50ms unblock under permit starvation).
//! 5. Multi-thread cancellation stampede (all blocked threads abort promptly).
//! 6. Spool footprint analytical calculation (+25% safety margin, copies multiplier, saturating arithmetic).
//! 7. Disk verdict threshold boundaries and edge conditions.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pdfcraft_print::governor::{
    calculate_required_spool_bytes, evaluate_disk_verdict, CancellationToken,
    ConcurrencyGovernor, CountingSemaphore, DiskVerdict, GovernorError,
    PrinterJobEstimate, SheetRasterMode,
};

#[test]
fn challenge_governor_default_permit_caps() {
    let governor = ConcurrencyGovernor::default_for_system();
    let sys_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);

    let expected_cpu = sys_cpus.clamp(1, 4);
    assert_eq!(
        governor.available_cpu_permits(),
        expected_cpu,
        "CPU permits must equal min(CPUs, 4)"
    );
    assert!(
        governor.available_cpu_permits() <= 4,
        "CPU permits must never exceed 4"
    );
    assert!(
        governor.available_cpu_permits() >= 1,
        "CPU permits must be at least 1"
    );

    assert_eq!(
        governor.available_spool_permits(),
        2,
        "Spool permits must strictly equal 2"
    );
}

#[test]
fn challenge_permit_caps_under_high_concurrency() {
    let cpu_cap = 3;
    let spool_cap = 2;
    let governor = Arc::new(ConcurrencyGovernor::new(cpu_cap, spool_cap));
    let token = CancellationToken::new();

    let active_cpu = Arc::new(AtomicUsize::new(0));
    let max_observed_cpu = Arc::new(AtomicUsize::new(0));

    let active_spool = Arc::new(AtomicUsize::new(0));
    let max_observed_spool = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();

    // Spawn 20 threads competing for CPU permits
    for _ in 0..20 {
        let gov = governor.clone();
        let tok = token.clone();
        let act = active_cpu.clone();
        let max_obs = max_observed_cpu.clone();

        handles.push(std::thread::spawn(move || {
            for _ in 0..50 {
                let guard = gov.acquire_cpu_permit(&tok).expect("acquire cpu");
                let current = act.fetch_add(1, Ordering::SeqCst) + 1;
                max_obs.fetch_max(current, Ordering::SeqCst);

                assert!(
                    current <= cpu_cap,
                    "VIOLATION: Concurrent CPU permits ({}) exceeded cap ({})!",
                    current,
                    cpu_cap
                );

                std::thread::sleep(Duration::from_micros(100));

                act.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }
        }));
    }

    // Spawn 15 threads competing for Spool permits
    for _ in 0..15 {
        let gov = governor.clone();
        let tok = token.clone();
        let act = active_spool.clone();
        let max_obs = max_observed_spool.clone();

        handles.push(std::thread::spawn(move || {
            for _ in 0..50 {
                let guard = gov.acquire_spool_permit(&tok).expect("acquire spool");
                let current = act.fetch_add(1, Ordering::SeqCst) + 1;
                max_obs.fetch_max(current, Ordering::SeqCst);

                assert!(
                    current <= spool_cap,
                    "VIOLATION: Concurrent Spool permits ({}) exceeded cap ({})!",
                    current,
                    spool_cap
                );

                std::thread::sleep(Duration::from_micros(100));

                act.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join");
    }

    assert_eq!(
        governor.available_cpu_permits(),
        cpu_cap,
        "All CPU permits must be cleanly restored"
    );
    assert_eq!(
        governor.available_spool_permits(),
        spool_cap,
        "All Spool permits must be cleanly restored"
    );
    assert!(
        max_observed_cpu.load(Ordering::SeqCst) <= cpu_cap,
        "Max concurrent CPU permits must not exceed cap"
    );
    assert!(
        max_observed_spool.load(Ordering::SeqCst) <= spool_cap,
        "Max concurrent Spool permits must not exceed cap"
    );
}

#[test]
fn challenge_counting_semaphore_release_clamp() {
    let sem = CountingSemaphore::new(3);
    assert_eq!(sem.available_permits(), 3);

    // Releasing beyond initial capacity must NOT inflate available permits
    sem.release();
    sem.release();
    sem.release();

    assert_eq!(
        sem.available_permits(),
        3,
        "Permit count must be strictly clamped to max_permits"
    );
}

#[test]
fn challenge_panic_safety_and_clean_unwinding_restoration() {
    let governor = Arc::new(ConcurrencyGovernor::new(2, 2));
    let token = CancellationToken::new();

    // Spawn 4 threads that acquire permits and then panic intentionally
    let mut handles = Vec::new();
    for i in 0..4 {
        let gov = governor.clone();
        let tok = token.clone();
        handles.push(std::thread::spawn(move || {
            if i % 2 == 0 {
                let _guard = gov.acquire_cpu_permit(&tok).unwrap();
                panic!("intentional panic holding cpu permit");
            } else {
                let _guard = gov.acquire_spool_permit(&tok).unwrap();
                panic!("intentional panic holding spool permit");
            }
        }));
    }

    for h in handles {
        let _ = h.join(); // All are expected to panic
    }

    // Both semaphores must have full permits restored by RAII Drop during unwinding
    assert_eq!(
        governor.available_cpu_permits(),
        2,
        "CPU permits must be fully restored after panics"
    );
    assert_eq!(
        governor.available_spool_permits(),
        2,
        "Spool permits must be fully restored after panics"
    );

    // Ensure normal acquisition works immediately without poison error or lock deadlock
    let g_cpu = governor.acquire_cpu_permit(&token);
    assert!(g_cpu.is_ok(), "CPU permit acquire must succeed after panics");
    let g_spool = governor.acquire_spool_permit(&token);
    assert!(g_spool.is_ok(), "Spool permit acquire must succeed after panics");
}

#[test]
fn challenge_cancellation_latency_under_permit_starvation() {
    let governor = Arc::new(ConcurrencyGovernor::new(1, 1));
    let token = CancellationToken::new();

    // Hold the single CPU permit
    let _holding_guard = governor.acquire_cpu_permit(&token).unwrap();
    assert_eq!(governor.available_cpu_permits(), 0);

    let gov_clone = governor.clone();
    let thread_token = token.clone();

    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();

    let blocked_thread = std::thread::spawn(move || {
        started_clone.store(true, Ordering::SeqCst);
        gov_clone.acquire_cpu_permit(&thread_token).map(|_| ())
    });

    // Wait until the thread has started attempting acquire
    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    // Give it 15ms to be firmly sleeping on the condvar
    std::thread::sleep(Duration::from_millis(15));

    // Cancel token and measure exact latency
    let cancel_start = Instant::now();
    token.cancel();

    let res = blocked_thread.join().expect("blocked thread join");
    let latency = cancel_start.elapsed();

    assert_eq!(
        res.err(),
        Some(GovernorError::Cancelled),
        "Starved thread must return GovernorError::Cancelled"
    );

    // The condvar wait_timeout is 50ms, so cancellation must be recognized within < 65ms (accounting for scheduling jitter)
    assert!(
        latency < Duration::from_millis(65),
        "Cancellation latency too high: {:?} (expected < 65ms)",
        latency
    );
}

#[test]
fn challenge_multi_thread_cancellation_stampede() {
    let governor = Arc::new(ConcurrencyGovernor::new(1, 1));
    let token = CancellationToken::new();

    // Hold the single permit
    let _held = governor.acquire_cpu_permit(&token).unwrap();

    let num_blocked = 10;
    let mut handles = Vec::new();

    for _ in 0..num_blocked {
        let gov = governor.clone();
        let tok = token.clone();
        handles.push(std::thread::spawn(move || {
            gov.acquire_cpu_permit(&tok).map(|_| ())
        }));
    }

    // Allow all threads to block on semaphore
    std::thread::sleep(Duration::from_millis(20));

    let cancel_start = Instant::now();
    token.cancel();

    for h in handles {
        let res = h.join().expect("thread join");
        assert_eq!(res.err(), Some(GovernorError::Cancelled));
    }

    let total_stampede_latency = cancel_start.elapsed();
    assert!(
        total_stampede_latency < Duration::from_millis(100),
        "Multi-thread cancellation stampede took too long: {:?}",
        total_stampede_latency
    );
}

#[test]
fn challenge_spool_analytical_calculation_properties() {
    // 1. Vector: 350 KB per sheet
    let job_vector = vec![PrinterJobEstimate::new(100, 1, SheetRasterMode::Vector)];
    let bytes_vec = calculate_required_spool_bytes(&job_vector);
    // 100 * 350 * 1024 = 35,840,000 bytes. * 1.25 (+25%) = 44,800,000 bytes
    assert_eq!(bytes_vec, 44_800_000);

    // 2. Raster 300 DPI: 18 MB per sheet
    let job_raster = vec![PrinterJobEstimate::new(10, 1, SheetRasterMode::Raster300Dpi)];
    let bytes_raster = calculate_required_spool_bytes(&job_raster);
    // 10 * 18 * 1024 * 1024 = 188,743,680 bytes. * 1.25 = 235,929,600 bytes
    assert_eq!(bytes_raster, 235_929_600);

    // 3. Multi-copy multiplier: 20 sheets * 3 copies = 60 sheets
    let job_copies = vec![PrinterJobEstimate::new(20, 3, SheetRasterMode::Vector)];
    let bytes_copies = calculate_required_spool_bytes(&job_copies);
    // 60 * 350 * 1024 = 21,504,000 bytes. * 1.25 = 26,880,000 bytes
    assert_eq!(bytes_copies, 26_880_000);

    // 4. Copies = 0 clamped to 1
    let job_zero_copies = PrinterJobEstimate::new(10, 0, SheetRasterMode::Vector);
    assert_eq!(job_zero_copies.copies, 1);

    // 5. Empty job list returns 0
    assert_eq!(calculate_required_spool_bytes(&[]), 0);

    // 6. Saturating arithmetic check: near u64::MAX should not panic
    let huge_custom = vec![PrinterJobEstimate::new(
        1,
        1,
        SheetRasterMode::CustomBytes(u64::MAX / 2),
    )];
    let huge_res = calculate_required_spool_bytes(&huge_custom);
    assert!(huge_res > 0, "Saturating arithmetic must not panic or wrap");
}

#[test]
fn challenge_disk_verdict_threshold_transitions() {
    let required: u64 = 200 * 1024 * 1024; // 200 MB

    // 1. Critical Exhaustion: available < required
    assert!(matches!(
        evaluate_disk_verdict(required - 1, required),
        DiskVerdict::CriticalExhaustion { available_mb: 199, estimated_mb: 200 }
    ));

    // 2. LowSpaceWarning boundary: available == required
    assert!(matches!(
        evaluate_disk_verdict(required, required),
        DiskVerdict::LowSpaceWarning { available_mb: 200, estimated_mb: 200 }
    ));

    // 3. Headroom threshold: required + 100MB = 300MB.
    // 1.5x multiplier: 200 * 1.5 = 300MB.
    // At 299 MB, it must be LowSpaceWarning
    assert!(matches!(
        evaluate_disk_verdict(299 * 1024 * 1024, required),
        DiskVerdict::LowSpaceWarning { .. }
    ));

    // At 301 MB, it has > 100MB headroom AND > 1.5x, so it must be Sufficient
    assert_eq!(
        evaluate_disk_verdict(301 * 1024 * 1024, required),
        DiskVerdict::Sufficient
    );

    // Large job: required = 1000 MB.
    // required + 100MB = 1100 MB.
    // But 1.5x required = 1500 MB!
    let large_req: u64 = 1000 * 1024 * 1024;
    // At 1200 MB (> required + 100MB, but < 1.5x), it must still be LowSpaceWarning!
    assert!(matches!(
        evaluate_disk_verdict(1200 * 1024 * 1024, large_req),
        DiskVerdict::LowSpaceWarning { .. }
    ));

    // At 1501 MB (> 1.5x), it transitions to Sufficient!
    assert_eq!(
        evaluate_disk_verdict(1501 * 1024 * 1024, large_req),
        DiskVerdict::Sufficient
    );
}
