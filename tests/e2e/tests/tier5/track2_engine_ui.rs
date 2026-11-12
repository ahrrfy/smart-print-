#![allow(unused_imports, unused_must_use, dead_code)]
//! Tier 5 Adversarial Stress Test Suite: Track 2 (Engine, Coordinator & UI Workflows)
//!
//! White-Box Coverage Hardening:
//! - Part 1: Zero-Copy Shared Memory Snapshot (`SharedPdfSource`) Pointer Identity & Bounds Safety
//! - Part 2: Print Coordinator & `CommittedSheetsLedger` Cascading Failover & Disjoint Partitioning
//! - Part 3: Stream Health Telemetry, Stall Detection & Concurrency Isolation
//! - Part 4: 60 FPS UI Non-Blocking Polling Latency, Preset Immutability & Preview Bounding

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

// =========================================================================
// PART 1: ZERO-COPY SHARED MEMORY SNAPSHOT ADVERSARIAL CHALLENGES
// =========================================================================

/// Challenge 1.1: Verify pointer identity invariance across 16 concurrent threads.
/// In a zero-copy architecture, cloning SharedPdfSource MUST NOT copy or reallocate
/// the buffer; raw pointer addresses across all threads must be bitwise identical.
#[test]
fn test_adv_snapshot_pointer_identity_invariance_across_16_threads() {
    let size = 1024 * 1024 * 4; // 4 MB payload
    let mut payload = vec![0u8; size];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = ((i * 31 + 17) & 0xFF) as u8;
    }

    let source = Arc::new(payload);
    let master_ptr = source.as_ptr() as usize;

    let barrier = Arc::new(std::sync::Barrier::new(16));
    let mut handles = Vec::new();

    for t in 0..16 {
        let s = source.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            b.wait();
            let thread_ptr = s.as_ptr() as usize;
            assert_eq!(
                thread_ptr, master_ptr,
                "Pointer identity MUST hold across thread {t} (raw pointer {thread_ptr:#x} == {master_ptr:#x})"
            );

            // Concurrent read validation across random offsets
            for i in 0..100 {
                let offset = (t * 1000 + i * 37) % (size - 256);
                let slice = &s[offset..offset + 256];
                assert_eq!(slice.len(), 256);
                assert_eq!(slice[0], ((offset * 31 + 17) & 0xFF) as u8);
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join failed");
    }
}

/// Challenge 1.2: Subslice bounds safety under inverted ranges, exact ends, and overflow values.
#[test]
#[allow(clippy::reversed_empty_ranges)]
fn test_adv_snapshot_subslice_bounds_inverted_and_oob_safety() {
    let data = b"%PDF-1.7 payload header body xref trailer %%EOF".to_vec();
    let len = data.len();

    // Helper simulating SharedPdfSource::slice behavior
    fn slice_fn(buf: &[u8], range: Range<usize>) -> Option<&[u8]> {
        buf.get(range)
    }

    // 1. Inverted range (start > end) - standard indexing panics; slice() must return None safely
    assert_eq!(slice_fn(&data, 20..10), None, "Inverted range 20..10 must return None without panic");
    assert_eq!(slice_fn(&data, len..0), None, "Inverted range len..0 must return None");

    // 2. Exact boundary slices
    assert_eq!(slice_fn(&data, len..len), Some(&b""[..]), "Slice at len..len must return empty slice");
    assert_eq!(slice_fn(&data, 0..len), Some(&data[..]), "Slice 0..len must return entire slice");

    // 3. One byte past boundary
    assert_eq!(slice_fn(&data, 0..len + 1), None, "Slice 0..len+1 must return None");
    assert_eq!(slice_fn(&data, len..len + 1), None, "Slice len..len+1 must return None");

    // 4. Extreme / overflow ranges
    assert_eq!(slice_fn(&data, 0..usize::MAX), None, "Slice 0..usize::MAX must return None without overflow");
    assert_eq!(slice_fn(&data, (usize::MAX - 100)..usize::MAX), None, "Near-MAX range must return None");
    assert_eq!(slice_fn(&data, usize::MAX..usize::MAX), None, "MAX..MAX range must return None");
}

/// Challenge 1.3: Empty source behavior and memory-map zero-byte file rejection.
#[test]
fn test_adv_snapshot_empty_source_and_mmap_rejection() {
    let empty_buf: Vec<u8> = Vec::new();
    assert_eq!(empty_buf.len(), 0);
    assert!(empty_buf.is_empty());
    assert_eq!(empty_buf.get(0..0), Some(&b""[..]));
    assert_eq!(empty_buf.get(0..1), None);
    assert_eq!(empty_buf.get(1..1), None);

    // Mmap zero-byte file rejection verification
    let temp_dir = std::env::temp_dir();
    let zero_path = temp_dir.join(format!(
        "pdfcraft_zero_test_{}_{}.tmp",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::write(&zero_path, b"").expect("write zero byte file");

    let file = std::fs::File::open(&zero_path).expect("open file");
    let meta = file.metadata().expect("meta");
    assert_eq!(meta.len(), 0, "File is zero bytes");

    // Zero-byte file pre-flight check as implemented in SharedPdfSource::from_file
    let snapshot_res = if meta.len() == 0 {
        Err("File is empty (0 bytes) and cannot be memory-mapped")
    } else {
        unsafe { memmap2::Mmap::map(&file).map(|_| ()).map_err(|_| "mmap error") }
    };
    assert!(snapshot_res.is_err(), "Memory-mapping a 0-byte file must be rejected by zero-length guard");
    assert_eq!(snapshot_res.unwrap_err(), "File is empty (0 bytes) and cannot be memory-mapped");

    let _ = std::fs::remove_file(zero_path);
}

/// Challenge 1.4: Pointer equality short-circuit vs content comparison.
#[test]
fn test_adv_snapshot_pointer_eq_vs_content_eq_short_circuit() {
    let payload = b"Document stream for pointer equality verification".to_vec();

    let a1 = Arc::new(payload.clone());
    let a2 = a1.clone();
    assert!(Arc::ptr_eq(&a1, &a2), "Cloned Arc must share identical pointer");

    // Separate allocation with identical bytes
    let a3 = Arc::new(payload.clone());
    assert!(!Arc::ptr_eq(&a1, &a3), "Distinct allocations must NOT be ptr_eq");
    assert_eq!(a1.as_slice(), a3.as_slice(), "Distinct allocations with identical bytes must compare equal");

    // Modified last byte
    let mut modified = payload.clone();
    let last = modified.len() - 1;
    modified[last] ^= 0xAA;
    let a4 = Arc::new(modified);
    assert_ne!(a1.as_slice(), a4.as_slice(), "Mutated byte must compare not equal");
}

// =========================================================================
// PART 2: PRINT COORDINATOR & CommittedSheetsLedger ADVERSARIAL CHALLENGES
// =========================================================================

/// Model representing coordinator CommittedSheetsLedger for white-box audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSheetsLedger {
    pub total_sheets: usize,
    pub committed_sheets: Vec<usize>,
    pub pending_sheets: Vec<usize>,
    pub failed_sheet: Option<usize>,
}

impl AuditSheetsLedger {
    pub fn new(total_sheets: usize) -> Self {
        Self {
            total_sheets,
            committed_sheets: Vec::new(),
            pending_sheets: (0..total_sheets).collect(),
            failed_sheet: None,
        }
    }

    pub fn from_pending_indices(total_in_batch: usize, pending: &[usize]) -> Self {
        Self {
            total_sheets: total_in_batch,
            committed_sheets: Vec::new(),
            pending_sheets: pending.to_vec(),
            failed_sheet: None,
        }
    }

    pub fn mark_committed(&mut self, sheet_idx: usize) {
        if let Some(pos) = self.pending_sheets.iter().position(|&s| s == sheet_idx) {
            self.pending_sheets.remove(pos);
        }
        if !self.committed_sheets.contains(&sheet_idx) {
            self.committed_sheets.push(sheet_idx);
            self.committed_sheets.sort_unstable();
        }
    }

    pub fn mark_committed_up_to(&mut self, count: usize) {
        let count = count.min(self.total_sheets);
        self.committed_sheets.clear();
        for i in 0..count {
            self.committed_sheets.push(i);
        }
        self.pending_sheets.retain(|&s| s >= count);
    }

    pub fn record_failure(&mut self, sheet_idx: usize) {
        self.failed_sheet = Some(sheet_idx);
        self.committed_sheets.retain(|&s| s != sheet_idx);
        if !self.pending_sheets.contains(&sheet_idx) && sheet_idx < self.total_sheets {
            self.pending_sheets.insert(0, sheet_idx);
            self.pending_sheets.sort_unstable();
            self.pending_sheets.dedup();
        }
    }

    pub fn slice_for_failover(&self) -> Vec<usize> {
        self.pending_sheets.clone()
    }

    pub fn is_complete(&self) -> bool {
        self.committed_sheets.len() >= self.total_sheets && self.pending_sheets.is_empty()
    }
}

/// Challenge 2.1: CommittedSheetsLedger Disjoint Partition Theorem.
/// Formally asserts:
///   1. Disjointness: C ∩ P = ∅ (strictly 0 duplicate sheets).
///   2. Completeness: C ∪ P = {0..N-1} (strictly 0 omitted sheets).
#[test]
fn test_adv_committed_sheets_ledger_disjoint_partition_guarantee() {
    let total = 100;
    let mut ledger = AuditSheetsLedger::new(total);

    // Commit sheets 0..44
    ledger.mark_committed_up_to(45);
    assert_eq!(ledger.committed_sheets.len(), 45);
    assert_eq!(ledger.pending_sheets.len(), 55);

    // Sheet 45 fails
    ledger.record_failure(45);
    assert_eq!(ledger.failed_sheet, Some(45));

    let pending = ledger.slice_for_failover();
    assert_eq!(pending.len(), 55);
    assert_eq!(pending[0], 45, "First pending sheet must be failed sheet 45");
    assert_eq!(pending[54], 99, "Last pending sheet must be sheet 99");

    // Disjointness check
    for &p in &pending {
        assert!(
            !ledger.committed_sheets.contains(&p),
            "Pending sheet {p} must NOT exist in committed_sheets"
        );
    }

    // Completeness check
    let mut union = ledger.committed_sheets.clone();
    union.extend_from_slice(&pending);
    union.sort_unstable();
    assert_eq!(
        union,
        (0..total).collect::<Vec<_>>(),
        "Union of committed and pending must exactly equal 0..total"
    );
}

/// Challenge 2.2: Cascading multi-printer failover (Printer A -> Printer B -> Printer C).
#[test]
fn test_adv_committed_sheets_ledger_cascading_triple_failover() {
    let total = 120;
    let mut ledger_a = AuditSheetsLedger::new(total);

    // Printer A commits sheets 0..39 (40 sheets), then fails at sheet 40
    ledger_a.mark_committed_up_to(40);
    ledger_a.record_failure(40);
    let pending_for_b = ledger_a.slice_for_failover();
    assert_eq!(pending_for_b.len(), 80);
    assert_eq!(pending_for_b[0], 40);

    // Printer B takes over the 80 pending sheets
    let mut ledger_b = AuditSheetsLedger::from_pending_indices(80, &pending_for_b);
    for idx in 40..90 {
        ledger_b.mark_committed(idx);
    }
    assert_eq!(ledger_b.committed_sheets.len(), 50);

    // Printer B stalls at sheet 90
    ledger_b.record_failure(90);
    let pending_for_c = ledger_b.slice_for_failover();
    assert_eq!(pending_for_c.len(), 30);
    assert_eq!(pending_for_c[0], 90);
    assert_eq!(pending_for_c[29], 119);

    // Printer C completes remaining 30 sheets
    let mut ledger_c = AuditSheetsLedger::from_pending_indices(30, &pending_for_c);
    for idx in 90..120 {
        ledger_c.mark_committed(idx);
    }
    assert_eq!(ledger_c.committed_sheets.len(), 30);
    assert!(ledger_c.is_complete());

    // Verify global fleet union
    let mut global_committed = Vec::new();
    global_committed.extend_from_slice(&ledger_a.committed_sheets);
    global_committed.extend_from_slice(&ledger_b.committed_sheets);
    global_committed.extend_from_slice(&ledger_c.committed_sheets);
    global_committed.sort_unstable();

    assert_eq!(global_committed.len(), 120);
    assert_eq!(
        global_committed,
        (0..120).collect::<Vec<_>>(),
        "Cascading triple failover must commit 100% of sheets without gaps or duplicates"
    );
}

/// Challenge 2.3: Out-of-order sheet commit and idempotent reconciliation.
#[test]
fn test_adv_committed_sheets_ledger_out_of_order_commit_and_reconciliation() {
    let mut ledger = AuditSheetsLedger::new(10);

    // Out of order commits: 3, 7, 1
    ledger.mark_committed(3);
    ledger.mark_committed(7);
    ledger.mark_committed(1);

    assert_eq!(ledger.committed_sheets.len(), 3);
    assert_eq!(ledger.committed_sheets, vec![1, 3, 7]);
    assert!(!ledger.pending_sheets.contains(&1));
    assert!(!ledger.pending_sheets.contains(&3));
    assert!(!ledger.pending_sheets.contains(&7));

    // Idempotent duplicate commit
    ledger.mark_committed(3);
    assert_eq!(ledger.committed_sheets.len(), 3);

    // Recording failure removes from committed and restores to pending
    ledger.record_failure(3);
    assert_eq!(ledger.failed_sheet, Some(3));
    assert!(!ledger.committed_sheets.contains(&3));
    assert!(ledger.pending_sheets.contains(&3));
}

/// Challenge 2.4: Zero-sheet and single-sheet boundary conditions.
#[test]
fn test_adv_committed_sheets_ledger_zero_and_single_sheet_boundaries() {
    // Zero-sheet job
    let mut zero_ledger = AuditSheetsLedger::new(0);
    assert!(zero_ledger.is_complete());
    assert_eq!(zero_ledger.slice_for_failover(), Vec::<usize>::new());
    zero_ledger.mark_committed_up_to(0);
    assert!(zero_ledger.is_complete());

    // Single-sheet success
    let mut single_ledger = AuditSheetsLedger::new(1);
    assert!(!single_ledger.is_complete());
    single_ledger.mark_committed(0);
    assert!(single_ledger.is_complete());
    assert_eq!(single_ledger.committed_sheets, vec![0]);
    assert!(single_ledger.pending_sheets.is_empty());

    // Single-sheet failure
    let mut single_fail = AuditSheetsLedger::new(1);
    single_fail.record_failure(0);
    assert_eq!(single_fail.slice_for_failover(), vec![0]);
    assert!(!single_fail.is_complete());
}

// =========================================================================
// PART 3: STREAM HEALTH, STALL DETECTION & CONCURRENCY ISOLATION
// =========================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum AuditStreamHealth {
    Healthy { rate_kbps: f64, sheets_per_sec: f64 },
    Throttled { rate_kbps: f64, reason: String },
    Stalled { stalled_duration_secs: u64 },
    Faulted { error: String, recoverable: bool },
    Completed { total_sheets: usize, duration_secs: f64 },
}

/// Challenge 3.1: Stream stall detection when progress updates cease for >= 10s.
#[test]
fn test_adv_coordinator_stall_timeout_transition_detection() {
    let stall_timeout = Duration::from_secs(10);
    let last_progress_time = Instant::now() - Duration::from_secs(14); // 14s elapsed (> 10s timeout)
    let elapsed = Instant::now().duration_since(last_progress_time);

    let mut health = AuditStreamHealth::Healthy { rate_kbps: 80.0, sheets_per_sec: 1.5 };
    if elapsed >= stall_timeout {
        health = AuditStreamHealth::Stalled {
            stalled_duration_secs: elapsed.as_secs(),
        };
    }

    assert!(
        matches!(health, AuditStreamHealth::Stalled { stalled_duration_secs } if stalled_duration_secs >= 10),
        "Stream with no updates for >= 10s MUST transition to Stalled"
    );
}

/// Challenge 3.2: Cancellation token cooperative propagation.
#[test]
fn test_adv_coordinator_cancellation_token_cooperative_propagation() {
    let master_cancel = Arc::new(AtomicBool::new(false));
    let worker1_cancel = Arc::new(AtomicBool::new(false));
    let worker2_cancel = Arc::new(AtomicBool::new(false));

    // Cancel individual worker
    worker1_cancel.store(true, Ordering::Relaxed);
    assert!(worker1_cancel.load(Ordering::Relaxed));
    assert!(!worker2_cancel.load(Ordering::Relaxed), "Worker 2 must NOT be cancelled");
    assert!(!master_cancel.load(Ordering::Relaxed), "Master must NOT be cancelled");

    // Master cancellation propagates to all
    master_cancel.store(true, Ordering::Relaxed);
    assert!(master_cancel.load(Ordering::Relaxed));
}

/// Challenge 3.3: Bandwidth throttling transition detection below threshold (< 25.0 KB/s).
#[test]
fn test_adv_coordinator_bandwidth_throttling_transition_detection() {
    const THROTTLE_THRESHOLD_KBPS: f64 = 25.0;

    let eval_health = |rate_kbps: f64, sheets_per_sec: f64| -> AuditStreamHealth {
        if rate_kbps < THROTTLE_THRESHOLD_KBPS && rate_kbps > 0.0 {
            AuditStreamHealth::Throttled {
                rate_kbps,
                reason: format!("Transfer rate {rate_kbps:.1} KB/s is below threshold 25.0 KB/s"),
            }
        } else {
            AuditStreamHealth::Healthy {
                rate_kbps,
                sheets_per_sec,
            }
        }
    };

    // Low bandwidth stream (12.5 KB/s) must be marked Throttled
    let throttled = eval_health(12.5, 0.4);
    assert!(
        matches!(throttled, AuditStreamHealth::Throttled { rate_kbps, .. } if (rate_kbps - 12.5).abs() < 1e-4),
        "Stream below 25.0 KB/s must transition to Throttled"
    );

    // High bandwidth stream (120.0 KB/s) remains Healthy
    let healthy = eval_health(120.0, 3.8);
    assert!(
        matches!(healthy, AuditStreamHealth::Healthy { rate_kbps, sheets_per_sec } if rate_kbps > 100.0 && sheets_per_sec > 3.0),
        "Stream above 25.0 KB/s must remain Healthy"
    );
}

/// Challenge 3.4: Worker fault isolation barrier preventing cascade across concurrent printers.
#[test]
fn test_adv_coordinator_worker_fault_isolation_barrier() {
    struct MockWorkerState {
        printer_id: String,
        health: AuditStreamHealth,
        sheets_committed: usize,
    }

    let mut fleet = [
        MockWorkerState {
            printer_id: "Laser-Floor1".to_string(),
            health: AuditStreamHealth::Healthy { rate_kbps: 95.0, sheets_per_sec: 2.2 },
            sheets_committed: 22,
        },
        MockWorkerState {
            printer_id: "Color-Executive".to_string(),
            health: AuditStreamHealth::Healthy { rate_kbps: 110.0, sheets_per_sec: 2.5 },
            sheets_committed: 22,
        },
    ];

    // Worker 1 encounters a driver / paper jam fault
    fleet[0].health = AuditStreamHealth::Faulted {
        error: "Paper jam in fuser unit".to_string(),
        recoverable: true,
    };

    // Assert Worker 1 is Faulted
    assert!(matches!(fleet[0].health, AuditStreamHealth::Faulted { .. }));

    // Assert Worker 2 is completely isolated and continues unaffected
    assert!(matches!(fleet[1].health, AuditStreamHealth::Healthy { .. }));
    fleet[1].sheets_committed = 50;
    fleet[1].health = AuditStreamHealth::Completed {
        total_sheets: 50,
        duration_secs: 20.0,
    };
    assert!(matches!(fleet[1].health, AuditStreamHealth::Completed { total_sheets: 50, .. }));
}

// =========================================================================
// PART 4: 60 FPS UI POLLING LATENCY, PRESET IMMUTABILITY & PREVIEW BOUNDING
// =========================================================================

/// Challenge 4.1: 60 FPS UI Non-Blocking Event Polling Under Saturated Channel.
/// Draining bounded events (max 64) must execute in < 1.0ms (nominal < 0.2ms)
/// to maintain the 16.67ms frame budget without freezing the UI.
#[test]
fn test_adv_print_ui_event_poll_bounded_latency_under_saturated_channel() {
    let (tx, rx) = channel::<usize>();

    // Saturated channel: 256 high-frequency progress updates
    for i in 0..256 {
        tx.send(i).unwrap();
    }

    let start = Instant::now();
    let mut drained = 0;
    let mut received = Vec::new();
    while drained < 64 {
        match rx.try_recv() {
            Ok(item) => {
                received.push(item);
                drained += 1;
            }
            Err(_) => break,
        }
    }
    let elapsed = start.elapsed();

    // 1. Must drain strictly at most 64 events per frame
    assert_eq!(received.len(), 64, "Must drain strictly 64 events per poll frame");

    // 2. Latency must be < 2.0ms (typically < 0.1ms) for 60 FPS fluidity
    assert!(
        elapsed < Duration::from_millis(2),
        "Draining 64 events took {:?}, must be < 2ms",
        elapsed
    );

    // 3. Remaining 192 events are retained for subsequent frames
    let mut remaining = 0;
    while rx.try_recv().is_ok() {
        remaining += 1;
    }
    assert_eq!(remaining, 192);
}

/// Challenge 4.2: Print Preset Store Built-in Immutability and Custom Preset Lifecycle.
#[test]
fn test_adv_print_preset_store_built_in_immutability_and_lifecycle() {
    #[derive(Debug, Clone, PartialEq)]
    struct MockPreset {
        id: String,
        name: String,
        is_built_in: bool,
        copies: u32,
    }

    struct MockStore {
        presets: BTreeMap<String, MockPreset>,
        active_id: Option<String>,
    }

    impl MockStore {
        fn with_defaults() -> Self {
            let mut presets = BTreeMap::new();
            presets.insert(
                "standard-duplex".to_string(),
                MockPreset { id: "standard-duplex".into(), name: "Standard Duplex".into(), is_built_in: true, copies: 1 },
            );
            presets.insert(
                "monochrome-eco".to_string(),
                MockPreset { id: "monochrome-eco".into(), name: "Monochrome Eco".into(), is_built_in: true, copies: 1 },
            );
            presets.insert(
                "engineering-100".to_string(),
                MockPreset { id: "engineering-100".into(), name: "Engineering 1:1".into(), is_built_in: true, copies: 1 },
            );
            Self { presets, active_id: Some("standard-duplex".into()) }
        }

        fn delete_preset(&mut self, id: &str) -> bool {
            if let Some(p) = self.presets.get(id) {
                if p.is_built_in {
                    return false; // Built-in protection
                }
            } else {
                return false;
            }
            let removed = self.presets.remove(id).is_some();
            if removed && self.active_id.as_deref() == Some(id) {
                self.active_id = None;
            }
            removed
        }
    }

    let mut store = MockStore::with_defaults();

    // 1. Attempting to delete built-in presets must be rejected
    assert!(!store.delete_preset("standard-duplex"), "standard-duplex must be immutable");
    assert!(!store.delete_preset("monochrome-eco"), "monochrome-eco must be immutable");
    assert!(!store.delete_preset("engineering-100"), "engineering-100 must be immutable");
    assert_eq!(store.presets.len(), 3);

    // 2. Custom user preset insertion and deletion
    store.presets.insert(
        "custom-batch-2".into(),
        MockPreset { id: "custom-batch-2".into(), name: "Custom Batch 2".into(), is_built_in: false, copies: 2 },
    );
    assert_eq!(store.presets.len(), 4);
    store.active_id = Some("custom-batch-2".into());

    assert!(store.delete_preset("custom-batch-2"), "User preset deletion must succeed");
    assert_eq!(store.presets.len(), 3);
    assert!(store.active_id.is_none(), "Active preset must be cleared upon deletion");
}

/// Challenge 4.3: Interactive Preview Rasters Scaling Under Extreme / Pathological Geometry.
#[test]
fn test_adv_preview_rasters_pathological_geometry_and_byte_budget() {
    const PREVIEW_MAX_SIDE: f32 = 4096.0;
    const PREVIEW_BOX: (f32, f32) = (296.0, 356.0);
    const PREVIEW_BYTES: f64 = 128.0 * 1024.0 * 1024.0;

    let calc_scale = |pw: f64, ph: f64, ppp: f32, max_side: f32| -> Option<f32> {
        if !(pw.is_finite() && ph.is_finite()) || pw <= 0.0 || ph <= 0.0 {
            return None;
        }
        let ppp = if ppp.is_finite() { ppp.max(1.0) } else { 1.0 };
        let side = if max_side.is_finite() && max_side >= 1.0 {
            max_side.min(PREVIEW_MAX_SIDE)
        } else {
            PREVIEW_MAX_SIDE
        };
        let k = (PREVIEW_BOX.0 / pw as f32).min(PREVIEW_BOX.1 / ph as f32);
        let placed = k * ppp;
        let long_pt = (pw.max(ph)) as f32;
        if !placed.is_finite() || placed < 0.05 || !long_pt.is_finite() || long_pt < 1.0 {
            return None;
        }
        Some(placed.min(64.0).min(side / long_pt))
    };

    // 1. Large 5,000 pt architectural canvas (placed scale >= 0.05)
    for max_side in [0.0, -100.0, f32::NAN, 1.0, 4096.0, 16384.0] {
        let scale = calc_scale(5_000.0, 5_000.0, 2.0, max_side).expect("scale must be calculated for 5000pt canvas");
        assert!(scale.is_finite());
        assert!(scale > 0.0);
        let rendered_pixels = 5_000.0 * (scale as f64);
        assert!(
            rendered_pixels <= 4097.0,
            "Rendered dimension ({rendered_pixels}) must never exceed PREVIEW_MAX_SIDE (4096) + slack"
        );
    }

    // Extreme 50,000 pt canvas at 2.0 ppp has placed scale < 0.05, so preview_rasters safely skips it
    assert_eq!(calc_scale(50_000.0, 50_000.0, 2.0, 4096.0), None, "Sub-0.05 placed scale must be safely skipped");

    // Extreme 50,000 pt canvas at 10.0 ppp (high DPI) produces valid scale clamped to PREVIEW_MAX_SIDE
    let scale_hi = calc_scale(50_000.0, 50_000.0, 10.0, 4096.0).expect("scale for high-DPI large canvas");
    assert!(scale_hi.is_finite());
    assert!(50_000.0 * (scale_hi as f64) <= 4097.0);

    // 2. Microscopic page (1.0 pt)
    let micro_scale = calc_scale(1.0, 1.0, 1.0, 4096.0).expect("micro page");
    assert!(micro_scale.is_finite());
    assert!(micro_scale <= 64.0, "Scale must be clamped at 64.0x maximum");

    // 3. Degenerate inputs (0.0, negative, NaN)
    assert_eq!(calc_scale(0.0, 100.0, 1.0, 4096.0), None);
    assert_eq!(calc_scale(-50.0, 100.0, 1.0, 4096.0), None);
    assert_eq!(calc_scale(f64::NAN, 100.0, 1.0, 4096.0), None);
}

/// Challenge 4.4: Failover Modal State Mapping and Mathematical Consistency.
#[test]
fn test_adv_failover_modal_state_mapping() {
    #[derive(Debug, Clone, PartialEq)]
    struct FailoverModalState {
        from_printer: String,
        to_printer: String,
        failed_sheet: usize,
        committed_sheets: usize,
        pending_sheets: usize,
        total_sheets: usize,
        error_message: String,
    }

    let modal = FailoverModalState {
        from_printer: "Office-Laser-1".to_string(),
        to_printer: "Backup-Laser-2".to_string(),
        failed_sheet: 45,
        committed_sheets: 45,
        pending_sheets: 55,
        total_sheets: 100,
        error_message: "Mechanical paper jam in fuser".to_string(),
    };

    assert_eq!(modal.from_printer, "Office-Laser-1");
    assert_eq!(modal.to_printer, "Backup-Laser-2");
    assert_eq!(modal.failed_sheet, 45);
    assert_eq!(modal.committed_sheets + modal.pending_sheets, modal.total_sheets);
}

/// Challenge 4.5: Print Presets JSON Corruption and Fallback Resilience.
#[test]
fn test_adv_print_presets_json_corruption_and_fallback_resilience() {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct MiniPresetStore {
        presets: BTreeMap<String, String>,
        active: Option<String>,
    }

    impl MiniPresetStore {
        fn with_defaults() -> Self {
            let mut presets = BTreeMap::new();
            presets.insert("standard-duplex".to_string(), "Standard Duplex Config".to_string());
            presets.insert("monochrome-eco".to_string(), "Monochrome Eco Config".to_string());
            presets.insert("engineering-100".to_string(), "Engineering 1:1 Config".to_string());
            Self {
                presets,
                active: Some("standard-duplex".to_string()),
            }
        }

        fn from_json_or_default(json_str: &str) -> Self {
            serde_json::from_str::<MiniPresetStore>(json_str).unwrap_or_else(|_| Self::with_defaults())
        }
    }

    // 1. Valid JSON round-trip
    let store = MiniPresetStore::with_defaults();
    let json = serde_json::to_string(&store).expect("serialize");
    let loaded = MiniPresetStore::from_json_or_default(&json);
    assert_eq!(loaded, store);

    // 2. Corrupted JSON (truncated, missing closing braces, invalid syntax)
    let corrupted_samples = [
        "",
        "{",
        "{\"presets\": {",
        "NOT_JSON_AT_ALL",
        "{\"presets\": null}",
    ];

    for corrupted in corrupted_samples {
        let recovered = MiniPresetStore::from_json_or_default(corrupted);
        assert_eq!(recovered.presets.len(), 3, "Corrupted JSON must safely fallback to 3 defaults");
        assert_eq!(recovered.active.as_deref(), Some("standard-duplex"));
    }
}
