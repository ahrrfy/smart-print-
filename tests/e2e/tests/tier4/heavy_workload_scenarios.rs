//! Tier 4 Real-World Workload Scenarios: Heavy Workload & Concurrency Governance
//!
//! Module: pdfcraft/tests/e2e/tests/tier4/heavy_workload_scenarios.rs
//!
//! Authoritative verification suite covering:
//! - Scenario 1: 500-Page Monolithic Manual Spooling & Bounded Memory (Feature 18)
//! - Scenario 2: 200MB Scan Zero-Copy Concurrency Across 3 Printers (Feature 17)
//! - Scenario 6: High-Throughput 4-Printer Simultaneous Dispatch Under 60 FPS UI Load (Feature 19)
//! - Scenario 13: 20-Job Burst Concurrency Throttling & Permit Governance (Feature 21)

use pdfcraft_e2e::benchmarks::{BenchmarkAssertions, MemoryProbe, UiFluidityMonitor};
use pdfcraft_e2e::mock_driver::{
    DuplexBinding, MockIppDriver, MockV3Driver, MockV3DriverConfig,
    MockV4Driver, MockV4DriverConfig, PrintableBounds,
    DMCOLOR_MONOCHROME, DMDUP_SIMPLEX, DM_COLOR, DM_DUPLEX, DM_IN_BUFFER, DM_ORIENTATION, DM_OUT_BUFFER,
};
use pdfcraft_e2e::mock_spooler::CommittedSheetsLedger;
use pdfcraft_print::governor::ConcurrencyGovernor;
use pdfcraft_print::stream::{
    bounded_stream_channel, CancellationToken, SheetBuffer, SheetData,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

// =========================================================================
// Opaque-Box Zero-Copy Shared Memory Model for Concurrency Testing
// =========================================================================

#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("File is empty (0 bytes) and cannot be memory-mapped")]
    EmptyFile,
    #[error("Invalid slice range {range:?} for source of length {len}")]
    OutOfBounds { range: Range<usize>, len: usize },
}

#[derive(Clone)]
pub enum SharedPdfSource {
    Mmap(Arc<memmap2::Mmap>),
    Heap(Arc<Vec<u8>>),
}

impl SharedPdfSource {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, SnapshotError> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 {
            return Err(SnapshotError::EmptyFile);
        }
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Self::Mmap(Arc::new(mmap)))
    }

    #[allow(dead_code)]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::Heap(Arc::new(bytes))
    }

    #[allow(dead_code)]
    pub fn from_arc(arc: Arc<Vec<u8>>) -> Self {
        Self::Heap(arc)
    }

    pub fn is_mmap(&self) -> bool {
        matches!(self, Self::Mmap(_))
    }

    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }

    pub fn as_slice(&self) -> &[u8] {
        self.as_ref()
    }
}

impl AsRef<[u8]> for SharedPdfSource {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mmap(m) => &m[..],
            Self::Heap(h) => &h[..],
        }
    }
}

impl Deref for SharedPdfSource {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

fn make_test_vector_sheet(sheet_idx: usize, total_sheets: usize, payload_size: usize) -> SheetBuffer {
    // Generate synthetic PDF vector stream payload
    let mut payload = Vec::with_capacity(payload_size + 32);
    payload.extend_from_slice(b"q 1 0 0 1 0 0 cm /DeviceGray cs 0.2 sc\n");
    while payload.len() + 32 < payload_size {
        payload.extend_from_slice(b"10 10 500 700 re f\n0.5 g 12 /F1 Tf\n");
    }
    if payload.len() < payload_size {
        payload.resize(payload_size, b' ');
    }
    payload.extend_from_slice(b"Q\n%%EOF\n");

    SheetBuffer {
        sheet_index: sheet_idx,
        total_sheets,
        width_pt: 595.28,
        height_pt: 841.89,
        data: SheetData::Vector(payload),
    }
}

// =========================================================================
// SCENARIO 1: 500-Page Monolithic Manual Spooling & Bounded Memory
// =========================================================================

#[test]
fn test_tier4_scenario_01_500_page_monolithic_manual_spooling_bounded_memory() {
    const TOTAL_PAGES: usize = 500;
    const SHEET_PAYLOAD_BYTES: usize = 100 * 1024; // 100 KB per page -> 50 MB total document
    const CHANNEL_CAPACITY: usize = 2;

    let (tx, rx) = bounded_stream_channel(CHANNEL_CAPACITY);
    let token = CancellationToken::new();
    let ledger = Arc::new(CommittedSheetsLedger::new(TOTAL_PAGES, CHANNEL_CAPACITY));
    let mock_driver = Arc::new(MockV3Driver::new(MockV3DriverConfig {
        printer_name: "Office_Heavy_V3_Laser".into(),
        bounds: PrintableBounds::a4_standard_margins(),
        ..Default::default()
    }));

    // Start background memory probe tracking RSS delta throughout the entire 500-page run
    let probe = MemoryProbe::start();

    // Producer thread: streams 500 pages one-by-one into bounded channel with backpressure
    let t_producer = token.clone();
    let producer_handle = std::thread::spawn(move || {
        for page_idx in 0..TOTAL_PAGES {
            if t_producer.is_cancelled() {
                break;
            }
            let sheet = make_test_vector_sheet(page_idx, TOTAL_PAGES, SHEET_PAYLOAD_BYTES);
            tx.send_with_backpressure(sheet, &t_producer)
                .expect("Producer send failed");
        }
    });

    // Spool consumer thread: processes sheets, validates DEVMODE parameters, and immediately drops buffers
    let ledger_consumer = Arc::clone(&ledger);
    let driver_consumer = Arc::clone(&mock_driver);
    let t_consumer = token.clone();

    let consumer_handle = std::thread::spawn(move || {
        let mut committed = 0;

        // Perform V3 DEVMODE initial negotiation
        let needed = driver_consumer.document_properties_pass1() as usize;
        let mut devmode_buf = vec![0u8; needed];
        driver_consumer
            .document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
            .expect("DEVMODE Pass 2 default populate");

        let dm_fields = DM_COLOR | DM_DUPLEX | DM_ORIENTATION;
        devmode_buf[72..76].copy_from_slice(&dm_fields.to_le_bytes());
        devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());
        devmode_buf[94..96].copy_from_slice(&DMDUP_SIMPLEX.to_le_bytes());

        let mut out_buf = vec![0u8; needed];
        driver_consumer
            .document_properties_pass2(
                Some(&devmode_buf),
                &mut out_buf,
                DM_IN_BUFFER | DM_OUT_BUFFER,
            )
            .expect("DEVMODE Pass 2 merge");

        while committed < TOTAL_PAGES {
            match rx.recv_with_cancellation(&t_consumer).expect("Receive sheet buffer") {
                Some(sheet_buf) => {
                    let idx = sheet_buf.sheet_index;
                    ledger_consumer
                        .begin_sheet(idx, 1)
                        .expect("Backpressure limit breached in ledger");

                    // Verify sheet metadata
                    assert_eq!(sheet_buf.total_sheets, TOTAL_PAGES);
                    assert!(sheet_buf.resident_bytes() >= SHEET_PAYLOAD_BYTES);

                    // Commit sheet and release sheet_buf immediately
                    ledger_consumer.commit_sheet(idx, sheet_buf.resident_bytes());
                    drop(sheet_buf); // Explicit O(1) buffer reclamation

                    committed += 1;
                }
                None => break,
            }
        }
        committed
    });

    producer_handle.join().expect("Producer thread join");
    let committed_total = consumer_handle.join().expect("Consumer thread join");

    let (_initial_rss, _peak_rss, delta_rss) = probe.stop();

    // Assertional Oracle 1: All 500 pages committed without loss or premature termination
    assert_eq!(committed_total, TOTAL_PAGES);
    assert_eq!(ledger.committed_count(), TOTAL_PAGES);
    assert!(ledger.is_all_committed());

    // Assertional Oracle 2: Bounded channel backpressure never exceeded capacity 2
    let peak_in_flight = ledger.peak_in_flight_observed();
    assert!(
        peak_in_flight <= CHANNEL_CAPACITY,
        "Channel backpressure breached: peak in-flight was {peak_in_flight} (max allowed {CHANNEL_CAPACITY})"
    );
    assert!(
        peak_in_flight >= 1,
        "Channel in-flight sheets must be observed during execution"
    );

    // Assertional Oracle 3: Memory footprint stays bounded O(1) with peak RSS delta < 35 MB
    BenchmarkAssertions::assert_peak_memory_delta(delta_rss, 35.0);

    // Assertional Oracle 4: Driver state validation
    mock_driver.assert_pass1_called();
    mock_driver.assert_monochrome_asserted();
    mock_driver.assert_duplex_asserted(DuplexBinding::Simplex);
    mock_driver.assert_driver_extra_preserved();
}

// =========================================================================
// SCENARIO 2: 200MB Scan Zero-Copy Concurrency Across 3 Printers
// =========================================================================

#[test]
fn test_tier4_scenario_02_200mb_scan_zerocopy_concurrency_three_printers() {
    const SCAN_SIZE_BYTES: usize = 200 * 1024 * 1024; // 200 MB scan file

    // 1. Prepare temporary 200 MB scan document on disk
    let temp_dir = tempfile::tempdir().expect("Create tempdir");
    let file_path = temp_dir.path().join("high_res_200mb_scan.pdf");

    {
        let mut file = File::create(&file_path).expect("Create 200MB file");
        file.write_all(b"%PDF-1.7\n% Scan Document 200MB\n").unwrap();
        // Write repeating non-zero pattern to ensure pages are physically committed
        let chunk = vec![0x7A; 1024 * 1024]; // 1 MB chunk
        for _ in 0..200 {
            file.write_all(&chunk).unwrap();
        }
        file.write_all(b"%%EOF\n").unwrap();
        file.sync_all().unwrap();
    }

    // 2. Memory-map the document as SharedPdfSource
    let source = SharedPdfSource::from_file(&file_path).expect("Mmap 200MB file");
    assert!(source.is_mmap(), "Source must be backed by OS virtual memory map");
    assert!(source.len() >= SCAN_SIZE_BYTES);
    let base_ptr = source.as_slice().as_ptr() as usize;

    // 3. Instantiate 3 distinct mock printers: V3 DEVMODE, V4 XPS PrintTicket, CUPS IPP
    let driver_v3 = Arc::new(MockV3Driver::new(MockV3DriverConfig {
        printer_name: "Floor1_V3_Production".into(),
        ..Default::default()
    }));
    let driver_v4 = Arc::new(MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Floor2_V4_XPS".into(),
        supports_color: true,
        supports_duplex: true,
        ..Default::default()
    }));
    let driver_ipp = Arc::new(MockIppDriver::new("Floor3_IPP_Network", true));

    // Barrier to synchronize simultaneous start of all 3 printer worker threads
    let start_barrier = Arc::new(Barrier::new(4));
    let probe = MemoryProbe::start();

    // Worker 1: Win32 V3 Driver
    let src1 = source.clone();
    let bar1 = Arc::clone(&start_barrier);
    let v3_clone = Arc::clone(&driver_v3);
    let h1 = std::thread::spawn(move || {
        bar1.wait();
        let ptr = src1.as_slice().as_ptr() as usize;

        // Exercise V3 DocumentProperties negotiation
        let needed = v3_clone.document_properties_pass1() as usize;
        let mut devmode_buf = vec![0u8; needed];
        v3_clone
            .document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
            .unwrap();
        devmode_buf[72..76].copy_from_slice(&(DM_COLOR | DM_DUPLEX).to_le_bytes());
        devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());
        devmode_buf[94..96].copy_from_slice(&DMDUP_SIMPLEX.to_le_bytes());

        let mut out_buf = vec![0u8; needed];
        v3_clone
            .document_properties_pass2(
                Some(&devmode_buf),
                &mut out_buf,
                DM_IN_BUFFER | DM_OUT_BUFFER,
            )
            .unwrap();

        // Touch pages across the mapped buffer to verify physical page access
        let sample = src1[100 * 1024 * 1024];
        assert_eq!(sample, 0x7A);

        (ptr, src1.len())
    });

    // Worker 2: Win32 V4 XPS Driver
    let src2 = source.clone();
    let bar2 = Arc::clone(&start_barrier);
    let v4_clone = Arc::clone(&driver_v4);
    let h2 = std::thread::spawn(move || {
        bar2.wait();
        let ptr = src2.as_slice().as_ptr() as usize;

        v4_clone.pt_open_provider().unwrap();
        let caps = v4_clone.pt_get_print_capabilities().unwrap();
        assert!(caps.contains("psf:PrintCapabilities"));

        let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
            <psf:Feature name="psk:PageOutputColor"><psf:Option name="psk:Monochrome"/></psf:Feature>
        </psf:PrintTicket>"#;
        let merged = v4_clone.pt_merge_and_validate(None, delta).unwrap();
        assert!(merged.contains("psk:Monochrome"));

        let sample = src2[150 * 1024 * 1024];
        assert_eq!(sample, 0x7A);

        (ptr, src2.len())
    });

    // Worker 3: CUPS / IPP 2.0 Driver
    let src3 = source.clone();
    let bar3 = Arc::clone(&start_barrier);
    let ipp_clone = Arc::clone(&driver_ipp);
    let h3 = std::thread::spawn(move || {
        bar3.wait();
        let ptr = src3.as_slice().as_ptr() as usize;

        let mut attrs = HashMap::new();
        attrs.insert("print-color-mode".to_string(), "monochrome".to_string());
        attrs.insert("sides".to_string(), "one-sided".to_string());

        let job_id = ipp_clone.submit_ipp_job(attrs, src3.as_slice()).unwrap();
        assert!(job_id >= 1001);

        let sample = src3[50 * 1024 * 1024];
        assert_eq!(sample, 0x7A);

        (ptr, src3.len())
    });

    // Release all 3 workers simultaneously
    start_barrier.wait();

    let (ptr1, len1) = h1.join().unwrap();
    let (ptr2, len2) = h2.join().unwrap();
    let (ptr3, len3) = h3.join().unwrap();

    let (_initial_rss, _peak_rss, delta_rss) = probe.stop();

    // Assertional Oracle 1: Pointer invariance across all 3 workers (0 duplicate memory allocation)
    assert_eq!(ptr1, base_ptr, "Worker 1 memory pointer diverged");
    assert_eq!(ptr2, base_ptr, "Worker 2 memory pointer diverged");
    assert_eq!(ptr3, base_ptr, "Worker 3 memory pointer diverged");
    assert_eq!(len1, source.len());
    assert_eq!(len2, source.len());
    assert_eq!(len3, source.len());

    // Assertional Oracle 2: Peak RSS growth <= 1.15 * doc_size (230 MB max, strictly NOT 3 * 200MB = 600MB)
    let doc_size_mb = (source.len() as f64) / (1024.0 * 1024.0);
    let max_allowed_delta_mb = 1.15 * doc_size_mb;
    BenchmarkAssertions::assert_peak_memory_delta(delta_rss, max_allowed_delta_mb);

    // Assertional Oracle 3: Driver-specific verifications
    driver_v3.assert_pass1_called();
    driver_v3.assert_monochrome_asserted();
    driver_v3.assert_driver_extra_preserved();

    driver_v4.assert_no_concurrency_violations();

    driver_ipp.assert_attribute("print-color-mode", "monochrome");
    driver_ipp.assert_attribute("sides", "one-sided");
    assert_eq!(driver_ipp.last_job().unwrap().pdf_stream_len, source.len());
}

// =========================================================================
// SCENARIO 6: High-Throughput 4-Printer Simultaneous Dispatch Under 60 FPS UI Load
// =========================================================================

#[test]
fn test_tier4_scenario_06_high_throughput_4printer_simultaneous_dispatch_60fps_fluidity() {
    const PRINTER_COUNT: usize = 4;
    const PAGES_PER_JOB: usize = 25;
    const TARGET_FPS: f64 = 60.0;

    let token = CancellationToken::new();
    let completed_jobs = Arc::new(AtomicUsize::new(0));
    let mut progress_receivers = Vec::new();

    // Spawn 4 concurrent background worker threads
    for printer_idx in 0..PRINTER_COUNT {
        let (prog_tx, prog_rx) = std::sync::mpsc::sync_channel::<usize>(PAGES_PER_JOB);
        progress_receivers.push(prog_rx);

        let t_clone = token.clone();
        let comp_clone = Arc::clone(&completed_jobs);

        std::thread::spawn(move || {
            let ledger = CommittedSheetsLedger::new(PAGES_PER_JOB, 2);

            for sheet_idx in 0..PAGES_PER_JOB {
                if t_clone.is_cancelled() {
                    break;
                }
                ledger.begin_sheet(sheet_idx, printer_idx).unwrap();

                // Simulate sheet spooling processing
                std::thread::sleep(Duration::from_millis(8));

                ledger.commit_sheet(sheet_idx, 1024);
                let _ = prog_tx.try_send(sheet_idx + 1);
            }

            comp_clone.fetch_add(1, Ordering::SeqCst);
        });
    }

    // Main UI Thread: simulates interactive 60 FPS event loop sampling fluidity
    let mut monitor = UiFluidityMonitor::new(TARGET_FPS);
    let mut max_polling_duration = Duration::ZERO;
    let mut frame_count = 0;

    while completed_jobs.load(Ordering::SeqCst) < PRINTER_COUNT {
        monitor.begin_frame();

        // Measure non-blocking channel polling latency across all 4 workers
        let poll_start = Instant::now();
        for rx in &progress_receivers {
            while let Ok(_progress) = rx.try_recv() {
                // Non-blocking drain of progress updates
            }
        }
        let poll_elapsed = poll_start.elapsed();
        if poll_elapsed > max_polling_duration {
            max_polling_duration = poll_elapsed;
        }

        // Simulate interactive UI rendering (e.g. egui widget drawing, panning)
        std::thread::sleep(Duration::from_millis(4));

        monitor.end_frame();
        frame_count += 1;

        // Frame pacing to maintain ~60 FPS budget
        std::thread::sleep(Duration::from_millis(10));
    }

    let report = monitor.finish();

    // Assertional Oracle 1: All 4 print jobs completed successfully in the background
    assert_eq!(completed_jobs.load(Ordering::SeqCst), PRINTER_COUNT);
    assert!(frame_count >= 5, "Expected multiple sampled UI frames, got {frame_count}");

    // Assertional Oracle 2: Zero freeze events (>100ms)
    assert_eq!(
        report.freeze_events, 0,
        "UI main thread experienced {} freeze event(s) >100ms",
        report.freeze_events
    );

    // Assertional Oracle 3: Zero severe stutter frames (>33.3ms)
    assert_eq!(
        report.stutter_frames, 0,
        "UI main thread experienced {} stutter frame(s) >33.3ms",
        report.stutter_frames
    );

    // Assertional Oracle 4: Zero dropped frames exceeding 60 FPS budget (16.67ms)
    assert_eq!(
        report.dropped_frames, 0,
        "UI main thread experienced {} dropped frame(s) >16.67ms",
        report.dropped_frames
    );

    // Assertional Oracle 5: Max frame duration strictly under 16.67 ms
    assert!(
        report.max_frame_duration_ms < 16.67,
        "Max frame duration exceeded 16.67ms: {:.2} ms",
        report.max_frame_duration_ms
    );

    // Assertional Oracle 6: Non-blocking channel polling latency <= 0.2 ms per frame
    let poll_ms = max_polling_duration.as_secs_f64() * 1000.0;
    assert!(
        poll_ms <= 0.20,
        "Non-blocking channel polling exceeded 0.2ms threshold: {:.4} ms",
        poll_ms
    );

    // Built-in benchmark oracle assertion
    BenchmarkAssertions::assert_ui_fluidity_60fps(&report);
}

// =========================================================================
// SCENARIO 13: 20-Job Burst Concurrency Throttling & Permit Governance
// =========================================================================

#[test]
fn test_tier4_scenario_13_20_job_burst_concurrency_throttling_permit_governance() {
    const TOTAL_JOBS: usize = 20;
    const MAX_CPU_PERMITS: usize = 4;
    const MAX_SPOOL_PERMITS: usize = 2;

    let governor = Arc::new(ConcurrencyGovernor::new(MAX_CPU_PERMITS, MAX_SPOOL_PERMITS));
    let token = CancellationToken::new();

    let active_cpu = Arc::new(AtomicUsize::new(0));
    let peak_cpu = Arc::new(AtomicUsize::new(0));
    let active_spool = Arc::new(AtomicUsize::new(0));
    let peak_spool = Arc::new(AtomicUsize::new(0));
    let completed_jobs = Arc::new(AtomicUsize::new(0));

    // Barrier synchronizing 20 jobs + 1 test coordinator to launch simultaneous burst contention
    let burst_barrier = Arc::new(Barrier::new(TOTAL_JOBS + 1));
    let mut worker_handles = Vec::new();

    for job_id in 0..TOTAL_JOBS {
        let g = Arc::clone(&governor);
        let t = token.clone();
        let barrier = Arc::clone(&burst_barrier);
        let a_cpu = Arc::clone(&active_cpu);
        let p_cpu = Arc::clone(&peak_cpu);
        let a_spool = Arc::clone(&active_spool);
        let p_spool = Arc::clone(&peak_spool);
        let comp = Arc::clone(&completed_jobs);

        worker_handles.push(std::thread::spawn(move || {
            // Wait for simultaneous burst release
            barrier.wait();

            // Phase 1: CPU Render Permit Acquisition (bounded <= 4)
            {
                let _cpu_guard = g.acquire_cpu_permit(&t).expect("Acquire CPU permit");
                let cur = a_cpu.fetch_add(1, Ordering::SeqCst) + 1;
                p_cpu.fetch_max(cur, Ordering::SeqCst);

                assert!(
                    cur <= MAX_CPU_PERMITS,
                    "CPU permits breached bound: active {cur} > limit {MAX_CPU_PERMITS}"
                );

                // Simulate CPU vector rasterization work
                std::thread::sleep(Duration::from_millis(8));

                a_cpu.fetch_sub(1, Ordering::SeqCst);
                // _cpu_guard dropped here, returning permit to semaphore
            }

            // Phase 2: Spool Submission Permit Acquisition (bounded <= 2)
            {
                let _spool_guard = g.acquire_spool_permit(&t).expect("Acquire Spool permit");
                let cur = a_spool.fetch_add(1, Ordering::SeqCst) + 1;
                p_spool.fetch_max(cur, Ordering::SeqCst);

                assert!(
                    cur <= MAX_SPOOL_PERMITS,
                    "Spool permits breached bound: active {cur} > limit {MAX_SPOOL_PERMITS}"
                );

                // Simulate driver spool submission I/O
                std::thread::sleep(Duration::from_millis(8));

                a_spool.fetch_sub(1, Ordering::SeqCst);
                // _spool_guard dropped here, returning permit to semaphore
            }

            comp.fetch_add(1, Ordering::SeqCst);
            job_id
        }));
    }

    // Trigger burst release of all 20 threads simultaneously
    burst_barrier.wait();

    for handle in worker_handles {
        handle.join().expect("Worker thread join");
    }

    // Assertional Oracle 1: All 20 jobs completed without thread deadlocks or starvation
    assert_eq!(completed_jobs.load(Ordering::SeqCst), TOTAL_JOBS);

    // Assertional Oracle 2: Active rendering threads never exceeded 4 simultaneously
    let observed_peak_cpu = peak_cpu.load(Ordering::SeqCst);
    assert!(
        observed_peak_cpu <= MAX_CPU_PERMITS,
        "Active CPU renders exceeded limit 4: observed {observed_peak_cpu}"
    );
    assert!(
        observed_peak_cpu >= 2,
        "Burst test was not concurrent enough: peak CPU was only {observed_peak_cpu}"
    );

    // Assertional Oracle 3: Active spool submissions never exceeded 2 simultaneously
    let observed_peak_spool = peak_spool.load(Ordering::SeqCst);
    assert!(
        observed_peak_spool <= MAX_SPOOL_PERMITS,
        "Active Spool submissions exceeded limit 2: observed {observed_peak_spool}"
    );
    assert!(
        observed_peak_spool >= 1,
        "Spool submissions must be observed during burst test"
    );

    // Assertional Oracle 4: Zero permit leakage; semaphores fully restored
    assert_eq!(
        governor.available_cpu_permits(),
        MAX_CPU_PERMITS,
        "CPU permits leaked: {} available, expected {}",
        governor.available_cpu_permits(),
        MAX_CPU_PERMITS
    );
    assert_eq!(
        governor.available_spool_permits(),
        MAX_SPOOL_PERMITS,
        "Spool permits leaked: {} available, expected {}",
        governor.available_spool_permits(),
        MAX_SPOOL_PERMITS
    );
}
