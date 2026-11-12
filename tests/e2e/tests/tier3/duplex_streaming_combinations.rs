//! Tier 3 Cross-Feature Pairwise Interaction Suite: Tests 10 to 18
//!
//! Module: pdfcraft/tests/e2e/tests/tier3/duplex_streaming_combinations.rs
//!
//! Exercises orthogonal interactions across:
//! - Duplex Mode: Simplex, Flip Long Edge Booklet, Flip Short Edge Tablet
//! - Orientation Alignment: Homogeneous, Conflicting mixed orientations, Odd page counts
//! - Streaming & Concurrency: Shared Memory Mmap/Arc, Bounded Backpressure Channel (capacity=2), Two-Tier Governor Permits

use pdfcraft_e2e::benchmarks::{BenchmarkAssertions, MemoryProbe};
use pdfcraft_e2e::margin_assertions::{
    calculate_page_transform, MarginAssertions, PageFitMode,
};
use pdfcraft_e2e::mock_driver::{
    DuplexBinding, HardwareMargins, MockIppDriver, MockV3Driver, MockV3DriverConfig,
    MockV4Driver, MockV4DriverConfig, PrintableBounds,
    DMDUP_HORIZONTAL, DMDUP_SIMPLEX, DMDUP_VERTICAL,
    DM_DUPLEX, DM_IN_BUFFER, DM_OUT_BUFFER,
};
use pdfcraft_e2e::mock_spooler::CommittedSheetsLedger;
use pdfcraft_print::duplex::{
    plan_duplex_sheets, DuplexBinding as PrintDuplex,
};
use pdfcraft_print::governor::{
    ConcurrencyGovernor, GovernorError,
};
use pdfcraft_print::stream::{
    bounded_stream_channel, CancellationToken, SheetBuffer, SheetData,
};
use std::collections::HashMap;
use std::fs::File;
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

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

#[allow(dead_code)]
#[derive(Clone)]
pub enum SharedPdfSource {
    Mmap(Arc<memmap2::Mmap>),
    Heap(Arc<Vec<u8>>),
}

#[allow(dead_code)]
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

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::Heap(Arc::new(bytes))
    }

    pub fn from_arc(arc: Arc<Vec<u8>>) -> Self {
        Self::Heap(arc)
    }

    pub fn is_mmap(&self) -> bool {
        matches!(self, Self::Mmap(_))
    }

    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }

    pub fn slice(&self, range: Range<usize>) -> Option<&[u8]> {
        let bytes = self.as_ref();
        if range.start <= range.end && range.end <= bytes.len() {
            Some(&bytes[range])
        } else {
            None
        }
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

fn make_test_sheet_buffer(idx: usize, total: usize, bytes: Vec<u8>) -> SheetBuffer {
    SheetBuffer {
        sheet_index: idx,
        total_sheets: total,
        width_pt: 595.28,
        height_pt: 841.89,
        data: SheetData::Vector(bytes),
    }
}

// =========================================================================
// TEST 10: Simplex × Shared Memory Mmap × Multi-Worker Concurrency
// =========================================================================

#[test]
fn test_tier3_10_simplex_mmap_zerocopy_multiworker_concurrency() {
    // 1. Prepare temporary PDF document backed by disk file
    let dir = tempfile::tempdir().expect("Create temp dir");
    let file_path = dir.path().join("simplex_doc.pdf");
    let payload = vec![0x42u8; 2_000_000]; // 2 MB synthetic document
    std::fs::write(&file_path, &payload).expect("Write file");

    let source = SharedPdfSource::from_file(&file_path).expect("Mmap file");
    assert!(source.is_mmap());
    let main_ptr = source.as_slice().as_ptr() as usize;

    // 2. Homogeneous 6-page portrait document
    let page_sizes = vec![
        (595.28, 841.89),
        (595.28, 841.89),
        (595.28, 841.89),
        (595.28, 841.89),
        (595.28, 841.89),
        (595.28, 841.89),
    ];

    // 3. Spawn 3 concurrent worker threads reading the same mapped snapshot
    let mut handles = Vec::new();
    for worker_id in 0..3 {
        let src_clone = source.clone();
        let sizes_clone = page_sizes.clone();
        handles.push(std::thread::spawn(move || {
            let worker_ptr = src_clone.as_slice().as_ptr() as usize;
            let sheets = plan_duplex_sheets(&sizes_clone, PrintDuplex::Simplex);
            assert_eq!(sheets.len(), 6);
            for (idx, s) in sheets.iter().enumerate() {
                assert_eq!(s.sheet_index, idx);
                assert_eq!(s.front_page, idx);
                assert_eq!(s.back_page, None);
                assert!(!s.back_needs_rotation);
                assert_eq!(s.back_rotation_deg, 0);
            }
            (worker_id, worker_ptr, sheets.len())
        }));
    }

    // 4. Assertional Oracle: Pointer identity across all threads & 0 RAM duplication
    for h in handles {
        let (w_id, w_ptr, count) = h.join().expect("Worker thread join");
        assert_eq!(w_ptr, main_ptr, "Worker {w_id} has divergent memory pointer");
        assert_eq!(count, 6);
    }

    // 5. Assertional Oracle: Simplex DEVMODE flag validation on mock driver
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;
    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();
    devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_SIMPLEX.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    ).unwrap();

    mock.assert_duplex_asserted(DuplexBinding::Simplex);
    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_duplex, Some(1));
}

// =========================================================================
// TEST 11: Simplex × Bounded Backpressure Channel × Mixed Orientations
// =========================================================================

#[test]
fn test_tier3_11_simplex_backpressure_mixed_orientations_streaming() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();
    let ledger = Arc::new(CommittedSheetsLedger::new(10, 2));
    let printable = PrintableBounds::a4_standard_margins();

    // 10 pages with alternating Portrait and Landscape
    let page_sizes = vec![
        (595.28, 841.89), // P0: Portrait
        (841.89, 595.28), // P1: Landscape
        (595.28, 841.89), // P2: Portrait
        (841.89, 595.28), // P3: Landscape
        (595.28, 841.89), // P4: Portrait
        (841.89, 595.28), // P5: Landscape
        (595.28, 841.89), // P6: Portrait
        (841.89, 595.28), // P7: Landscape
        (595.28, 841.89), // P8: Portrait
        (841.89, 595.28), // P9: Landscape
    ];

    let sheets = plan_duplex_sheets(&page_sizes, PrintDuplex::Simplex);
    assert_eq!(sheets.len(), 10);

    // Producer thread streaming 10 simplex sheets into bounded channel (capacity 2)
    let t_clone = token.clone();
    let producer = std::thread::spawn(move || {
        for s in sheets {
            let buf = make_test_sheet_buffer(s.sheet_index, 10, vec![0xEE; 1024]);
            tx.send_with_backpressure(buf, &t_clone).expect("Send sheet buffer");
        }
    });

    // Consumer receiving sheets with backpressure, validating orientation invariance
    let mut received_count = 0;
    while received_count < 10 {
        match rx.recv_with_cancellation(&token).expect("Receive sheet") {
            Some(sheet_buf) => {
                let idx = sheet_buf.sheet_index;
                ledger.begin_sheet(idx, 1).expect("Begin sheet in ledger");

                // In Simplex mode, orientation alignment MUST NOT apply duplex rotation
                let page_size = page_sizes[idx];
                let transform = calculate_page_transform(
                    page_size,
                    &printable,
                    PageFitMode::FitPrintableArea,
                    false, // Simplex has no back page
                    true,
                    DuplexBinding::Simplex,
                );

                assert_eq!(transform.rotation_deg, 0, "Simplex page {idx} rotated unexpectedly");
                assert!(!transform.clipping.is_clipped(1e-4));
                MarginAssertions::assert_within_printable(page_size, &printable, &transform);

                std::thread::sleep(Duration::from_millis(5)); // Simulate hardware spool latency
                ledger.commit_sheet(idx, sheet_buf.resident_bytes());
                received_count += 1;
            }
            None => break,
        }
    }

    producer.join().expect("Producer finished");

    // Assertional Oracle: Backpressure enforcement & full commitment
    assert_eq!(received_count, 10);
    assert!(ledger.is_all_committed());
    assert!(ledger.peak_in_flight_observed() <= 2, "Channel backpressure breached bound 2");
}

// =========================================================================
// TEST 12: Simplex × Concurrency Governor Permits × Odd Page Burst Metering
// =========================================================================

#[test]
fn test_tier3_12_simplex_governor_odd_pages_burst_metering() {
    let gov = Arc::new(ConcurrencyGovernor::new(4, 2));
    let peak_cpu = Arc::new(AtomicUsize::new(0));
    let active_cpu = Arc::new(AtomicUsize::new(0));
    let peak_spool = Arc::new(AtomicUsize::new(0));
    let active_spool = Arc::new(AtomicUsize::new(0));

    // 6 concurrent print jobs submitted simultaneously, each with 7 pages (odd count)
    let mut handles = Vec::new();
    for job_id in 0..6 {
        let g = Arc::clone(&gov);
        let p_cpu = Arc::clone(&peak_cpu);
        let a_cpu = Arc::clone(&active_cpu);
        let p_spool = Arc::clone(&peak_spool);
        let a_spool = Arc::clone(&active_spool);

        handles.push(std::thread::spawn(move || {
            let token = CancellationToken::new();
            let odd_page_sizes = vec![(595.28, 841.89); 7];

            // Tier 1: CPU Render Permit Acquisition
            {
                let _cpu_guard = g.acquire_cpu_permit(&token).expect("Acquire CPU permit");
                let cur_cpu = a_cpu.fetch_add(1, Ordering::SeqCst) + 1;
                p_cpu.fetch_max(cur_cpu, Ordering::SeqCst);

                let sheets = plan_duplex_sheets(&odd_page_sizes, PrintDuplex::Simplex);
                assert_eq!(sheets.len(), 7);
                for s in &sheets {
                    assert_eq!(s.back_page, None);
                    assert!(!s.back_needs_rotation);
                }

                std::thread::sleep(Duration::from_millis(8)); // Simulate rendering
                a_cpu.fetch_sub(1, Ordering::SeqCst);
            }

            // Tier 2: Spool Submission Permit Acquisition
            {
                let _spool_guard = g.acquire_spool_permit(&token).expect("Acquire Spool permit");
                let cur_spool = a_spool.fetch_add(1, Ordering::SeqCst) + 1;
                p_spool.fetch_max(cur_spool, Ordering::SeqCst);

                std::thread::sleep(Duration::from_millis(8)); // Simulate spool transmission
                a_spool.fetch_sub(1, Ordering::SeqCst);
            }

            job_id
        }));
    }

    for h in handles {
        h.join().expect("Worker thread join");
    }

    // Assertional Oracle: Governor concurrency limits strictly respected
    assert!(peak_cpu.load(Ordering::SeqCst) <= 4, "Active CPU renders exceeded limit 4");
    assert!(peak_spool.load(Ordering::SeqCst) <= 2, "Active Spool submissions exceeded limit 2");
    assert_eq!(gov.available_cpu_permits(), 4, "CPU permits not fully restored");
    assert_eq!(gov.available_spool_permits(), 2, "Spool permits not fully restored");
}

// =========================================================================
// TEST 13: Duplex Long Edge × Shared Memory Arc × Homogeneous Booklet
// =========================================================================

#[test]
fn test_tier3_13_duplex_long_shared_arc_homogeneous_booklet() {
    let payload = vec![0x77u8; 1_500_000];
    let arc_data = Arc::new(payload);
    let source = SharedPdfSource::from_arc(Arc::clone(&arc_data));
    let base_ptr = source.as_slice().as_ptr() as usize;

    // 8 pages homogeneous Portrait A4 booklet
    let page_sizes = vec![(595.28, 841.89); 8];
    let sheets = plan_duplex_sheets(&page_sizes, PrintDuplex::FlipLongEdge);

    // Assertional Oracle: Exact 4-sheet booklet pairing
    assert_eq!(sheets.len(), 4, "8 pages duplex must yield exactly 4 sheets");
    for (sheet_idx, s) in sheets.iter().enumerate() {
        assert_eq!(s.sheet_index, sheet_idx);
        assert_eq!(s.front_page, sheet_idx * 2);
        assert_eq!(s.back_page, Some(sheet_idx * 2 + 1));
        assert!(s.master_is_portrait);
        assert!(!s.back_needs_rotation, "Homogeneous portrait requires no rotation");
        assert_eq!(s.back_rotation_deg, 0);
    }

    // Concurrent reader thread verifying Arc pointer identity
    let s_clone = source.clone();
    let reader = std::thread::spawn(move || {
        let r_ptr = s_clone.as_slice().as_ptr() as usize;
        assert_eq!(r_ptr, base_ptr);
        s_clone.len()
    });
    assert_eq!(reader.join().unwrap(), 1_500_000);

    // Mock driver validation: V3 DEVMODE + V4 XPS PrintTicket + CUPS IPP
    let mock_v3 = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock_v3.document_properties_pass1() as usize;
    let mut devmode_buf = vec![0u8; needed];
    mock_v3.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();
    devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_VERTICAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock_v3.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    ).unwrap();

    mock_v3.assert_duplex_asserted(DuplexBinding::FlipLongEdge);
    let last = mock_v3.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_duplex, Some(2)); // DMDUP_VERTICAL

    let mock_v4 = MockV4Driver::new(MockV4DriverConfig {
        supports_duplex: true,
        ..Default::default()
    });
    mock_v4.pt_open_provider().unwrap();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
        <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
            <psf:Option name="psk:TwoSidedLongEdge"/>
        </psf:Feature>
    </psf:PrintTicket>"#;
    let merged = mock_v4.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:TwoSidedLongEdge"));

    let mock_ipp = MockIppDriver::new("Office_Laser", true);
    let mut attrs = HashMap::new();
    attrs.insert("sides".to_string(), "two-sided-long-edge".to_string());
    mock_ipp.submit_ipp_job(attrs, b"%PDF payload").unwrap();
    mock_ipp.assert_attribute("sides", "two-sided-long-edge");
}

// =========================================================================
// TEST 14: Duplex Long Edge × Bounded Backpressure × Mixed Orient Portrait Master
// =========================================================================

#[test]
fn test_tier3_14_duplex_long_backpressure_mixed_orient_portrait_master() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();
    let ledger = Arc::new(CommittedSheetsLedger::new(2, 2));
    let printable = PrintableBounds::a4_standard_margins();

    // 4 pages: P0 Portrait (Master), P1 Landscape (Conflicting Back), P2 Portrait, P3 Portrait
    let page_sizes = vec![
        (595.28, 841.89), // P0: Portrait
        (841.89, 595.28), // P1: Landscape (Back page!)
        (595.28, 841.89), // P2: Portrait
        (595.28, 841.89), // P3: Portrait
    ];

    let sheets = plan_duplex_sheets(&page_sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 2);

    // Assertional Oracle: Sheet 0 must rotate back page by 270° for Long Edge flip
    assert_eq!(sheets[0].sheet_index, 0);
    assert_eq!(sheets[0].front_page, 0);
    assert_eq!(sheets[0].back_page, Some(1));
    assert!(sheets[0].master_is_portrait);
    assert!(sheets[0].back_needs_rotation);
    assert_eq!(sheets[0].back_rotation_deg, 270);

    // Sheet 1 has matching portrait orientations -> 0° rotation
    assert!(!sheets[1].back_needs_rotation);
    assert_eq!(sheets[1].back_rotation_deg, 0);

    // Stream through bounded backpressure channel
    let t_clone = token.clone();
    let producer = std::thread::spawn(move || {
        for s in sheets {
            let buf = make_test_sheet_buffer(s.sheet_index, 2, vec![0x11; 512]);
            tx.send_with_backpressure(buf, &t_clone).unwrap();
        }
    });

    let probe = MemoryProbe::start();

    // Consumer drains Sheet 0, verifies transform geometric bounding
    let s0_buf = rx.recv_with_cancellation(&token).unwrap().unwrap();
    ledger.begin_sheet(0, 1).unwrap();

    let transform_back = calculate_page_transform(
        page_sizes[1],
        &printable,
        PageFitMode::FitPrintableArea,
        true, // is back page
        true, // master is portrait
        DuplexBinding::FlipLongEdge,
    );
    assert!(transform_back.rotation_deg == 90 || transform_back.rotation_deg == 270);
    assert!(!transform_back.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(
        (page_sizes[1].1, page_sizes[1].0),
        &printable,
        &transform_back,
    );
    ledger.commit_sheet(0, s0_buf.resident_bytes());

    // Drain Sheet 1
    let s1_buf = rx.recv_with_cancellation(&token).unwrap().unwrap();
    ledger.begin_sheet(1, 1).unwrap();
    ledger.commit_sheet(1, s1_buf.resident_bytes());

    producer.join().unwrap();
    let (_init, _peak, delta) = probe.stop();

    assert!(ledger.is_all_committed());
    assert!(ledger.peak_in_flight_observed() <= 2);
    BenchmarkAssertions::assert_peak_memory_delta(delta, 10.0);
}

// =========================================================================
// TEST 15: Duplex Long Edge × Concurrency Governor × Odd Page Dangling Cancel
// =========================================================================

#[test]
fn test_tier3_15_duplex_long_governor_odd_pages_cooperative_cancel() {
    let gov = Arc::new(ConcurrencyGovernor::new(2, 1));
    let token = CancellationToken::new();

    // 5 pages -> 3 physical sheets: [P0, P1], [P2, P3], [P4, None]
    let page_sizes = vec![(595.28, 841.89); 5];
    let sheets = plan_duplex_sheets(&page_sizes, PrintDuplex::FlipLongEdge);
    assert_eq!(sheets.len(), 3);

    // Assertional Oracle: Dangling final simplex sheet
    assert_eq!(sheets[2].front_page, 4);
    assert_eq!(sheets[2].back_page, None);
    assert!(!sheets[2].back_needs_rotation);
    assert_eq!(sheets[2].back_rotation_deg, 0);

    // Acquire permits and verify cooperative cancellation
    let g_clone = Arc::clone(&gov);
    let t_clone = token.clone();
    let worker = std::thread::spawn(move || {
        let cpu_guard = g_clone.acquire_cpu_permit(&t_clone).expect("CPU permit acquired");
        assert_eq!(g_clone.available_cpu_permits(), 1);

        // While holding CPU permit, wait for spool permit after cancellation
        while !t_clone.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }

        let spool_res = g_clone.acquire_spool_permit(&t_clone);
        assert!(matches!(spool_res, Err(GovernorError::Cancelled)));
        drop(cpu_guard);
    });

    std::thread::sleep(Duration::from_millis(15));
    token.cancel();
    assert!(token.is_cancelled());

    worker.join().expect("Worker finished cleanly upon cancellation");

    // Assertional Oracle: All permits cleanly restored with 0 leak
    assert_eq!(gov.available_cpu_permits(), 2);
    assert_eq!(gov.available_spool_permits(), 1);
}

// =========================================================================
// TEST 16: Duplex Short Edge × Shared Memory Mmap × Tablet Calendar Concurrency
// =========================================================================

#[test]
fn test_tier3_16_duplex_short_shared_mmap_tablet_calendar_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("calendar.pdf");
    let payload = vec![0x99u8; 1_000_000];
    std::fs::write(&file_path, &payload).unwrap();

    let source = SharedPdfSource::from_file(&file_path).unwrap();
    assert!(source.is_mmap());
    let origin_ptr = source.as_slice().as_ptr() as usize;

    // 10 pages homogeneous Landscape A4 calendar/presentation slides
    let page_sizes = vec![(841.89, 595.28); 10];
    let sheets = plan_duplex_sheets(&page_sizes, PrintDuplex::FlipShortEdge);
    assert_eq!(sheets.len(), 5);

    for s in &sheets {
        assert!(!s.master_is_portrait);
        assert!(!s.back_needs_rotation);
        assert_eq!(s.back_rotation_deg, 0);
    }

    // Dual-printer concurrent dispatch sharing identical memory buffer
    let s1 = source.clone();
    let s2 = source.clone();

    let p1_handle = std::thread::spawn(move || {
        let mock_v3 = MockV3Driver::new(MockV3DriverConfig::default());
        let needed = mock_v3.document_properties_pass1() as usize;
        let mut devmode_buf = vec![0u8; needed];
        mock_v3.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();
        devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
        devmode_buf[94..96].copy_from_slice(&DMDUP_HORIZONTAL.to_le_bytes()); // Flip Short Edge

        let mut out_buf = vec![0u8; needed];
        mock_v3.document_properties_pass2(
            Some(&devmode_buf),
            &mut out_buf,
            DM_IN_BUFFER | DM_OUT_BUFFER,
        ).unwrap();

        mock_v3.assert_duplex_asserted(DuplexBinding::FlipShortEdge);
        (s1.as_slice().as_ptr() as usize, mock_v3.last_submitted_devmode().unwrap().dm_duplex)
    });

    let p2_handle = std::thread::spawn(move || {
        let mock_v4 = MockV4Driver::new(MockV4DriverConfig {
            supports_duplex: true,
            ..Default::default()
        });
        mock_v4.pt_open_provider().unwrap();
        let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
            <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
                <psf:Option name="psk:TwoSidedShortEdge"/>
            </psf:Feature>
        </psf:PrintTicket>"#;
        let merged = mock_v4.pt_merge_and_validate(None, delta).unwrap();
        assert!(merged.contains("psk:TwoSidedShortEdge"));
        s2.as_slice().as_ptr() as usize
    });

    let (ptr_v3, dm_duplex) = p1_handle.join().unwrap();
    let ptr_v4 = p2_handle.join().unwrap();

    // Assertional Oracle: Pointer invariance across heterogeneous drivers & flag assertion
    assert_eq!(ptr_v3, origin_ptr);
    assert_eq!(ptr_v4, origin_ptr);
    assert_eq!(dm_duplex, Some(3)); // DMDUP_HORIZONTAL
}

// =========================================================================
// TEST 17: Duplex Short Edge × Bounded Backpressure × Mixed Orient Landscape Master
// =========================================================================

#[test]
fn test_tier3_17_duplex_short_backpressure_mixed_orient_landscape_master() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();
    let ledger = Arc::new(CommittedSheetsLedger::new(2, 2));

    let landscape_printable = PrintableBounds {
        printable_width_pt: 824.88,
        printable_height_pt: 578.40,
        physical_width_pt: 841.68,
        physical_height_pt: 595.20,
        dpi_x: 600,
        dpi_y: 600,
        margins: HardwareMargins::uniform(8.40),
    };

    // 4 pages: P0 Landscape (Master), P1 Portrait (Conflicting Back), P2 Landscape, P3 Landscape
    let page_sizes = vec![
        (841.89, 595.28), // P0: Landscape Master
        (595.28, 841.89), // P1: Portrait Back
        (841.89, 595.28), // P2: Landscape Master
        (841.89, 595.28), // P3: Landscape Back
    ];

    let sheets = plan_duplex_sheets(&page_sizes, PrintDuplex::FlipShortEdge);
    assert_eq!(sheets.len(), 2);

    // Assertional Oracle: Landscape Master + Portrait Back on FlipShortEdge requires 270° rotation
    assert_eq!(sheets[0].sheet_index, 0);
    assert_eq!(sheets[0].front_page, 0);
    assert_eq!(sheets[0].back_page, Some(1));
    assert!(!sheets[0].master_is_portrait);
    assert!(sheets[0].back_needs_rotation);
    assert_eq!(sheets[0].back_rotation_deg, 270);

    // Sheet 1 homogeneous landscape -> 0° rotation
    assert!(!sheets[1].back_needs_rotation);
    assert_eq!(sheets[1].back_rotation_deg, 0);

    // Stream into bounded channel
    let t_clone = token.clone();
    let producer = std::thread::spawn(move || {
        for s in sheets {
            let buf = make_test_sheet_buffer(s.sheet_index, 2, vec![0x33; 1024]);
            tx.send_with_backpressure(buf, &t_clone).unwrap();
        }
    });

    // Consumer validates geometric margin fit of 270° rotated back page
    let s0 = rx.recv_with_cancellation(&token).unwrap().unwrap();
    ledger.begin_sheet(0, 1).unwrap();

    let transform = calculate_page_transform(
        page_sizes[1],
        &landscape_printable,
        PageFitMode::FitPrintableArea,
        true,
        false, // master is landscape
        DuplexBinding::FlipShortEdge,
    );
    assert_eq!(transform.rotation_deg, 270);
    assert!(!transform.clipping.is_clipped(1e-4));
    MarginAssertions::assert_within_printable(
        (page_sizes[1].1, page_sizes[1].0),
        &landscape_printable,
        &transform,
    );
    ledger.commit_sheet(0, s0.resident_bytes());

    let s1 = rx.recv_with_cancellation(&token).unwrap().unwrap();
    ledger.begin_sheet(1, 1).unwrap();
    ledger.commit_sheet(1, s1.resident_bytes());

    producer.join().unwrap();
    assert!(ledger.is_all_committed());
    assert!(ledger.peak_in_flight_observed() <= 2);
}

// =========================================================================
// TEST 18: Duplex Short Edge × Concurrency Governor × Mixed Orient Burst
// =========================================================================

#[test]
fn test_tier3_18_duplex_short_governor_mixed_orient_burst_concurrency() {
    let gov = Arc::new(ConcurrencyGovernor::new(4, 2));
    let peak_cpu = Arc::new(AtomicUsize::new(0));
    let active_cpu = Arc::new(AtomicUsize::new(0));
    let peak_spool = Arc::new(AtomicUsize::new(0));
    let active_spool = Arc::new(AtomicUsize::new(0));
    let total_sheets_committed = Arc::new(AtomicUsize::new(0));

    // 8 concurrent jobs: each job has 6 pages with Portrait Master + Landscape Back on FlipShortEdge
    // Orientation rule: (true, FlipShortEdge) => 90° rotation!
    let mut handles = Vec::new();
    for job_id in 0..8 {
        let g = Arc::clone(&gov);
        let p_cpu = Arc::clone(&peak_cpu);
        let a_cpu = Arc::clone(&active_cpu);
        let p_spool = Arc::clone(&peak_spool);
        let a_spool = Arc::clone(&active_spool);
        let committed = Arc::clone(&total_sheets_committed);

        handles.push(std::thread::spawn(move || {
            let token = CancellationToken::new();
            let page_sizes = vec![
                (595.28, 841.89), // P0: Portrait Master
                (841.89, 595.28), // P1: Landscape Back (90° rotation!)
                (595.28, 841.89), // P2: Portrait Master
                (841.89, 595.28), // P3: Landscape Back (90° rotation!)
                (595.28, 841.89), // P4: Portrait Master
                (841.89, 595.28), // P5: Landscape Back (90° rotation!)
            ];

            // 1. CPU Render Stage
            let sheets = {
                let _guard = g.acquire_cpu_permit(&token).unwrap();
                let cur = a_cpu.fetch_add(1, Ordering::SeqCst) + 1;
                p_cpu.fetch_max(cur, Ordering::SeqCst);

                let planned = plan_duplex_sheets(&page_sizes, PrintDuplex::FlipShortEdge);
                assert_eq!(planned.len(), 3);
                for s in &planned {
                    assert!(s.master_is_portrait);
                    assert!(s.back_needs_rotation);
                    assert_eq!(s.back_rotation_deg, 90, "Short edge portrait master must rotate 90°");
                }

                std::thread::sleep(Duration::from_millis(6));
                a_cpu.fetch_sub(1, Ordering::SeqCst);
                planned
            };

            // 2. Spool Submission Stage
            {
                let _guard = g.acquire_spool_permit(&token).unwrap();
                let cur = a_spool.fetch_add(1, Ordering::SeqCst) + 1;
                p_spool.fetch_max(cur, Ordering::SeqCst);

                std::thread::sleep(Duration::from_millis(6));
                committed.fetch_add(sheets.len(), Ordering::SeqCst);
                a_spool.fetch_sub(1, Ordering::SeqCst);
            }

            job_id
        }));
    }

    for h in handles {
        h.join().expect("Worker thread join");
    }

    // Assertional Oracle: Governor limits & full 24-sheet commitment
    assert!(peak_cpu.load(Ordering::SeqCst) <= 4, "Active CPU renders exceeded limit 4");
    assert!(peak_spool.load(Ordering::SeqCst) <= 2, "Active Spool submissions exceeded limit 2");
    assert_eq!(total_sheets_committed.load(Ordering::SeqCst), 24); // 8 jobs * 3 sheets
    assert_eq!(gov.available_cpu_permits(), 4);
    assert_eq!(gov.available_spool_permits(), 2);
}
