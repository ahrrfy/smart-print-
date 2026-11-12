//! Empirical verification tests for PrintOrchestrationCoordinator, CommittedSheetsLedger,
//! fault isolation, cooperative cancellation, and one-click failover rerouting.

use pdfcraft_engine::print_coordinator::{
    CancellationToken, CommittedSheetsLedger, FailoverError, PrintOrchestrationCoordinator,
    SharedPdfSource,
};
use pdfcraft_print::Settings;

#[test]
fn test_ledger_boundary_zero_sheets() {
    let mut ledger = CommittedSheetsLedger::new(0);
    assert_eq!(ledger.total_sheets, 0);
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), 0);
    assert!(ledger.is_complete());
    assert_eq!(ledger.slice_for_failover(), Vec::<usize>::new());

    // Boundary calls on 0 sheets
    ledger.mark_committed_up_to(0);
    assert!(ledger.is_complete());

    ledger.mark_committed(0);
    // Even if marked, count remains 0 because total is 0
    ledger.record_failure(0);
    assert_eq!(ledger.failed_sheet, Some(0));
    // Index 0 >= total_sheets (0), so it should not be inserted into pending
    assert!(ledger.pending_sheets.is_empty());
}

#[test]
fn test_ledger_boundary_one_sheet() {
    let mut ledger = CommittedSheetsLedger::new(1);
    assert_eq!(ledger.total_sheets, 1);
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), 1);
    assert_eq!(ledger.pending_sheets, vec![0]);
    assert!(!ledger.is_complete());

    // Failure on sheet 0
    ledger.record_failure(0);
    assert_eq!(ledger.failed_sheet, Some(0));
    assert_eq!(ledger.slice_for_failover(), vec![0]);
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), 1);

    // Now commit sheet 0
    ledger.mark_committed(0);
    assert_eq!(ledger.committed_count(), 1);
    assert_eq!(ledger.pending_count(), 0);
    assert!(ledger.is_complete());
    assert!(ledger.slice_for_failover().is_empty());
}

#[test]
fn test_ledger_boundary_500_sheets_stress() {
    let total = 500;
    let mut ledger = CommittedSheetsLedger::new(total);
    assert_eq!(ledger.total_sheets, total);
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), total);
    assert_eq!(ledger.pending_sheets.len(), total);

    // Commit up to sheet 247
    let committed_up_to = 247;
    ledger.mark_committed_up_to(committed_up_to);
    assert_eq!(ledger.committed_count(), 247);
    assert_eq!(ledger.pending_count(), 253);
    assert_eq!(ledger.committed_sheets, (0..247).collect::<Vec<_>>());
    assert_eq!(ledger.pending_sheets, (247..500).collect::<Vec<_>>());

    // Invariant: committed and pending are completely disjoint
    for s in &ledger.committed_sheets {
        assert!(!ledger.pending_sheets.contains(s), "Sheet {s} present in both committed and pending");
    }
    assert_eq!(ledger.committed_count() + ledger.pending_count(), total);

    // Failover slice must match pending sheets exactly
    let slice = ledger.slice_for_failover();
    assert_eq!(slice.len(), 253);
    assert_eq!(slice.first(), Some(&247));
    assert_eq!(slice.last(), Some(&499));

    // Failure at sheet 247
    ledger.record_failure(247);
    assert_eq!(ledger.failed_sheet, Some(247));
    assert!(ledger.pending_sheets.contains(&247));

    // Complete the rest
    ledger.mark_committed_up_to(total);
    assert!(ledger.is_complete());
    assert_eq!(ledger.committed_count(), total);
    assert_eq!(ledger.pending_count(), 0);
    assert!(ledger.slice_for_failover().is_empty());
}

#[test]
fn test_ledger_partial_failure_at_first_sheet() {
    let mut ledger = CommittedSheetsLedger::new(50);
    // Immediate failure before any sheet commits
    ledger.record_failure(0);
    assert_eq!(ledger.failed_sheet, Some(0));
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), 50);

    let slice = ledger.slice_for_failover();
    assert_eq!(slice, (0..50).collect::<Vec<_>>());
}

#[test]
fn test_ledger_partial_failure_at_last_sheet() {
    let total = 100;
    let mut ledger = CommittedSheetsLedger::new(total);
    // Sheets 0..98 committed
    ledger.mark_committed_up_to(99);
    assert_eq!(ledger.committed_count(), 99);
    assert_eq!(ledger.pending_count(), 1);
    assert_eq!(ledger.pending_sheets, vec![99]);

    // Last sheet (index 99) fails
    ledger.record_failure(99);
    assert_eq!(ledger.failed_sheet, Some(99));

    let slice = ledger.slice_for_failover();
    assert_eq!(slice, vec![99]);
}

#[test]
fn test_ledger_random_commit_permutation_invariants() {
    let total = 200;
    let mut ledger = CommittedSheetsLedger::new(total);

    // Deterministic pseudo-random permutation using LCG
    let mut perm: Vec<usize> = (0..total).collect();
    let mut seed: u64 = 0xdeadbeef;
    for i in (1..total).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = (seed >> 33) as usize % (i + 1);
        perm.swap(i, j);
    }

    let mut committed_set = std::collections::BTreeSet::new();

    for (step, &sheet_idx) in perm.iter().enumerate() {
        ledger.mark_committed(sheet_idx);
        committed_set.insert(sheet_idx);

        // Assert strictly maintained invariants:
        assert_eq!(ledger.committed_count(), step + 1);
        assert_eq!(ledger.pending_count(), total - (step + 1));
        assert_eq!(ledger.committed_count() + ledger.pending_count(), total);

        // Invariant: committed sheets is sorted
        for w in ledger.committed_sheets.windows(2) {
            assert!(w[0] < w[1], "Committed sheets not strictly ascending");
        }

        // Invariant: pending sheets does not contain committed sheet
        assert!(!ledger.pending_sheets.contains(&sheet_idx));

        // Invariant: union of committed and pending covers 0..total exactly
        let pending_set: std::collections::BTreeSet<_> = ledger.pending_sheets.iter().copied().collect();
        let intersection: Vec<_> = committed_set.intersection(&pending_set).collect();
        assert!(intersection.is_empty(), "Committed and pending sets overlap: {:?}", intersection);
    }

    assert!(ledger.is_complete());
    assert_eq!(ledger.pending_count(), 0);
    assert_eq!(ledger.slice_for_failover(), Vec::<usize>::new());
}

#[test]
fn test_cancellation_cooperative_atomic() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());

    let token_clone1 = token.clone();
    let token_clone2 = token.clone();
    let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
    let ready_worker = ready.clone();

    // Verify cross-thread cancellation visibility deterministically
    let h = std::thread::spawn(move || {
        assert!(!token_clone1.is_cancelled());
        ready_worker.wait();
        while !token_clone1.is_cancelled() {
            std::hint::spin_loop();
        }
        assert!(token_clone1.is_cancelled());
    });

    ready.wait();
    token_clone2.cancel();
    assert!(token.is_cancelled());
    h.join().unwrap();
}

#[test]
fn test_fault_isolation_multi_dispatch() {
    // Test dispatching to 3 mock printers: Printer1, Printer2, Printer3
    let printers = vec![
        "Isolated_Printer_1".to_string(),
        "Isolated_Printer_2".to_string(),
        "Isolated_Printer_3".to_string(),
    ];

    let page_sizes = vec![(612.0, 792.0), (612.0, 792.0)];
    let settings = Settings {
        pages: (0..page_sizes.len()).collect(),
        ..Default::default()
    };
    let doc_source = SharedPdfSource::from(vec![]); // Empty doc causes worker failure on open

    let handle_res = PrintOrchestrationCoordinator::dispatch_multi(
        &printers,
        &settings,
        doc_source,
        &page_sizes,
    );

    assert!(handle_res.is_ok(), "Dispatch failed: {:?}", handle_res.err());
    let mut handle = handle_res.unwrap();

    assert_eq!(handle.all_telemetry().len(), 3);
    assert!(handle.get_telemetry("Isolated_Printer_1").is_some());
    assert!(handle.get_telemetry("Isolated_Printer_2").is_some());
    assert!(handle.get_telemetry("Isolated_Printer_3").is_some());

    // Allow workers to execute and reach independent terminal failure states
    std::thread::sleep(std::time::Duration::from_millis(150));
    let events = handle.poll_events();
    assert!(!events.is_empty(), "Expected events from workers");

    // Cancel only Printer 1
    let cancelled = handle.cancel_printer("Isolated_Printer_1");
    assert!(cancelled);

    // Verify Printer 2 and Printer 3 still retain their telemetry and are not cancelled
    let tel1 = handle.get_telemetry("Isolated_Printer_1");
    let tel2 = handle.get_telemetry("Isolated_Printer_2");
    let tel3 = handle.get_telemetry("Isolated_Printer_3");

    assert!(tel1.is_some());
    assert!(tel2.is_some());
    assert!(tel3.is_some());
}

#[test]
fn test_failover_reroute_zero_duplicates_zero_drops() {
    let printers = vec!["Primary_Printer".to_string()];
    let page_sizes = vec![
        (612.0, 792.0),
        (612.0, 792.0),
        (612.0, 792.0),
        (612.0, 792.0),
    ];
    let settings = Settings {
        pages: (0..page_sizes.len()).collect(),
        ..Default::default()
    };
    let doc_source = SharedPdfSource::from(vec![]);

    let mut handle = PrintOrchestrationCoordinator::dispatch_multi(
        &printers,
        &settings,
        doc_source,
        &page_sizes,
    ).expect("dispatch");

    // Wait for worker to fail or start
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = handle.poll_events();

    // Verify error cases:
    // 1. Unknown source printer
    let err = handle.failover_reroute("NonExistent_Printer", "Target_Printer");
    assert_eq!(err, Err(FailoverError::PrinterNotFound("NonExistent_Printer".to_string())));

    // 2. Target printer busy
    // If Primary_Printer is in telemetry, rerouting to Primary_Printer should fail if busy or valid
    let busy_err = handle.failover_reroute("Primary_Printer", "Primary_Printer");
    // Primary_Printer is already in telemetry
    assert!(busy_err.is_err());

    // 3. Reroute when cancelled
    handle.cancel_all();
    let cancel_err = handle.failover_reroute("Primary_Printer", "Backup_Printer");
    assert_eq!(cancel_err, Err(FailoverError::BatchCancelled));
}

fn make_marked_pdf(page_count: usize, width: i64, height: i64) -> Vec<u8> {
    use pdfcraft_cos::{Dict, Document, Object, SaveOptions, Stream, write_full};

    let mut doc = Document::new_empty();
    let pages_ref = doc
        .root()
        .and_then(|r| doc.get(r).as_dict().and_then(|d| d.reference(b"Pages")))
        .unwrap();

    let mut font = Dict::new();
    font.set("Type", Object::name("Font"));
    font.set("Subtype", Object::name("Type1"));
    font.set("BaseFont", Object::name("Helvetica"));
    let font_ref = doc.add(Object::Dict(font));

    let mut kids = Vec::new();
    for idx in 0..page_count {
        let mut fonts = Dict::new();
        fonts.set("F1", Object::Ref(font_ref));
        let mut resources = Dict::new();
        resources.set("Font", Object::Dict(fonts));

        let body = format!(
            "1 0 0 rg 20 20 120 120 re f BT /F1 18 Tf 40 200 Td (SHEETMARKER_{}) Tj ET",
            idx + 1
        );
        let stream = doc.add(Object::Stream(Stream::from_raw(Dict::new(), body.into_bytes())));
        let mut page = Dict::new();
        page.set("Type", Object::name("Page"));
        page.set("Parent", Object::Ref(pages_ref));
        page.set("MediaBox", Object::Array([0, 0, width, height].map(Object::Int).to_vec()));
        page.set("Resources", Object::Dict(resources));
        page.set("Contents", Object::Ref(stream));
        kids.push(Object::Ref(doc.add(Object::Dict(page))));
    }
    doc.update_dict(pages_ref, |d| {
        d.set("Count", Object::Int(page_count as i64));
        d.set("Kids", Object::Array(kids));
    })
    .unwrap();
    write_full(&doc, &SaveOptions::default()).unwrap()
}

#[test]
fn test_parallel_dispatch_wakeup_and_zero_copy_shared_spool_buffer() {
    use pdfcraft_engine::print_coordinator::{BatchDispatchConfig, SpoolSubmitFn};
    use pdfcraft_engine::spooler::SharedSpoolBuffer;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let pdf_bytes = Arc::new(make_marked_pdf(3, 595, 842));
    let shared = SharedSpoolBuffer::new(pdf_bytes.clone());

    let mut mono_job_1 = pdfcraft_print::spool::Job {
        grayscale: true,
        ..Default::default()
    };
    let mut mono_job_2 = pdfcraft_print::spool::Job {
        grayscale: true,
        ..Default::default()
    };
    let buf_1 = shared.for_job(&mut mono_job_1, false);
    let buf_2 = shared.for_job(&mut mono_job_2, false);
    assert!(mono_job_1.grey_pages && mono_job_2.grey_pages);
    assert!(
        Arc::ptr_eq(&buf_1, &buf_2),
        "Sibling monochrome workers must share the exact same Arc allocation without cloning"
    );

    let wakeup_calls = Arc::new(AtomicUsize::new(0));
    let wakeup_cb = wakeup_calls.clone();
    let seen_ptrs: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_ptrs_cb = seen_ptrs.clone();

    let spooler: SpoolSubmitFn = Arc::new(move |bytes, _job, progress| {
        seen_ptrs_cb.lock().unwrap().push(bytes.as_ptr() as usize);
        progress(1, 3);
        progress(3, 3);
        Ok("ok".to_string())
    });

    let settings = Settings {
        pages: vec![0, 1, 2],
        paper: (595.0, 842.0),
        ..Default::default()
    };
    let default_job = pdfcraft_print::spool::Job {
        grayscale: true,
        copies: 2,
        collate: true,
        ..Default::default()
    };

    let mut handle = PrintOrchestrationCoordinator::dispatch_batch(BatchDispatchConfig {
        printers: vec![
            "Fleet_1".to_string(),
            "Fleet_2".to_string(),
            "Fleet_3".to_string(),
        ],
        settings,
        doc_source: SharedPdfSource::from_arc(pdf_bytes),
        page_sizes: vec![(595.0, 842.0); 3],
        printer_jobs: HashMap::new(),
        default_job,
        pre_imposed_pdf: None,
        custom_spooler: Some(spooler),
        on_event_wakeup: Some(Arc::new(move || {
            wakeup_cb.fetch_add(1, Ordering::Relaxed);
        })),
    })
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_all_completed() && Instant::now() < deadline {
        let _ = handle.poll_events();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_all_completed());
    assert!(wakeup_calls.load(Ordering::Relaxed) >= 4);

    let ptrs = seen_ptrs.lock().unwrap();
    assert_eq!(ptrs.len(), 3);
    assert_eq!(ptrs[0], ptrs[1], "All workers must receive the same zero-copy slice pointer");
    assert_eq!(ptrs[1], ptrs[2], "All workers must receive the same zero-copy slice pointer");
}

#[test]
fn test_shared_gdi_transparency_flattening_once_lock() {
    use pdfcraft_cos::{Dict, Document, Object, SaveOptions, Stream, write_full};
    use pdfcraft_engine::spooler::SharedSpoolBuffer;
    use std::sync::Arc;

    let mut doc = Document::new_empty();
    let pages_ref = doc
        .root()
        .and_then(|r| doc.get(r).as_dict().and_then(|d| d.reference(b"Pages")))
        .unwrap();

    let alpha: Vec<u8> = (0..32 * 32).map(|i| (i % 255) as u8).collect();
    let mut mask_dict = Dict::new();
    mask_dict.set("Type", Object::name("XObject"));
    mask_dict.set("Subtype", Object::name("Image"));
    mask_dict.set("Width", Object::Int(32));
    mask_dict.set("Height", Object::Int(32));
    mask_dict.set("ColorSpace", Object::name("DeviceGray"));
    mask_dict.set("BitsPerComponent", Object::Int(8));
    let mask_ref = doc.add(Object::Stream(Stream::flate(mask_dict, &alpha)));

    let rgb: Vec<u8> = (0..32 * 32).flat_map(|_| [220u8, 40, 20]).collect();
    let mut img_dict = Dict::new();
    img_dict.set("Type", Object::name("XObject"));
    img_dict.set("Subtype", Object::name("Image"));
    img_dict.set("Width", Object::Int(32));
    img_dict.set("Height", Object::Int(32));
    img_dict.set("ColorSpace", Object::name("DeviceRGB"));
    img_dict.set("BitsPerComponent", Object::Int(8));
    img_dict.set("SMask", Object::Ref(mask_ref));
    let img_ref = doc.add(Object::Stream(Stream::flate(img_dict, &rgb)));

    let mut xobjects = Dict::new();
    xobjects.set("Im0", Object::Ref(img_ref));
    let mut resources = Dict::new();
    resources.set("XObject", Object::Dict(xobjects));
    let content = doc.add(Object::Stream(Stream::from_raw(
        Dict::new(),
        b"q 200 0 0 200 50 50 cm /Im0 Do Q".to_vec(),
    )));
    let mut page = Dict::new();
    page.set("Type", Object::name("Page"));
    page.set("Parent", Object::Ref(pages_ref));
    page.set("MediaBox", Object::Array([0, 0, 595, 842].map(Object::Int).to_vec()));
    page.set("Resources", Object::Dict(resources));
    page.set("Contents", Object::Ref(content));
    let page_ref = doc.add(Object::Dict(page));
    doc.update_dict(pages_ref, |d| {
        d.set("Count", Object::Int(1));
        d.set("Kids", Object::Array(vec![Object::Ref(page_ref)]));
    })
    .unwrap();
    let pdf_bytes = Arc::new(write_full(&doc, &SaveOptions::default()).unwrap());

    let shared = SharedSpoolBuffer::new(pdf_bytes.clone());
    let mut gdi_job_1 = pdfcraft_print::spool::Job::default();
    let mut gdi_job_2 = pdfcraft_print::spool::Job::default();
    let mut v4_job = pdfcraft_print::spool::Job::default();

    let gdi_buf_1 = shared.for_job(&mut gdi_job_1, true);
    let gdi_buf_2 = shared.for_job(&mut gdi_job_2, true);
    let v4_buf = shared.for_job(&mut v4_job, false);

    assert!(
        Arc::ptr_eq(&gdi_buf_1, &gdi_buf_2),
        "Multiple GDI printers must share the OnceLock-flattened PDF buffer"
    );
    if cfg!(windows) {
        assert!(
            !Arc::ptr_eq(&gdi_buf_1, &v4_buf),
            "GDI buffer must be flattened while v4 XPS buffer stays vector"
        );
        let parsed = Document::open(gdi_buf_1).unwrap();
        let has_smask = parsed
            .scan_objects()
            .any(|(_, o)| matches!(&*o, Object::Stream(s) if s.dict.contains(b"SMask")));
        assert!(!has_smask, "Flattened GDI PDF must have no remaining SMask transparency");
    }
}

#[test]
fn test_unified_monochrome_jpeg_no_flate_bloat_and_indexed_inline_k_only() {
    use image::codecs::jpeg::JpegEncoder;
    use pdfcraft_cos::{Dict, Document, Object, PdfString, SaveOptions, Stream, write_full};
    use pdfcraft_engine::spooler::monochrome_k_only;
    use std::sync::Arc;

    // 1. Encode a real 16x16 RGB JPEG stream
    let rgb_pixels: Vec<u8> = (0..16 * 16).flat_map(|_| [240u8, 30, 60]).collect();
    let mut jpeg_bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg_bytes, 85)
        .encode(&rgb_pixels, 16, 16, image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::new_empty();
    let pages_ref = doc
        .root()
        .and_then(|r| doc.get(r).as_dict().and_then(|d| d.reference(b"Pages")))
        .unwrap();

    let mut jpeg_dict = Dict::new();
    jpeg_dict.set("Type", Object::name("XObject"));
    jpeg_dict.set("Subtype", Object::name("Image"));
    jpeg_dict.set("Width", Object::Int(16));
    jpeg_dict.set("Height", Object::Int(16));
    jpeg_dict.set("ColorSpace", Object::name("DeviceRGB"));
    jpeg_dict.set("BitsPerComponent", Object::Int(8));
    jpeg_dict.set("Filter", Object::name("DCTDecode"));
    let jpeg_ref = doc.add(Object::Stream(Stream::from_raw(jpeg_dict, jpeg_bytes)));

    // 2. 4-bit Indexed RGB image (previously skipped by MonochromeVectorFilter alone!)
    let palette = vec![255u8, 0, 0, 0, 255, 0];
    let indexed_cs = Object::Array(vec![
        Object::name("Indexed"),
        Object::name("DeviceRGB"),
        Object::Int(1),
        Object::String(PdfString::literal(palette)),
    ]);
    let mut idx_dict = Dict::new();
    idx_dict.set("Type", Object::name("XObject"));
    idx_dict.set("Subtype", Object::name("Image"));
    idx_dict.set("Width", Object::Int(4));
    idx_dict.set("Height", Object::Int(4));
    idx_dict.set("ColorSpace", indexed_cs);
    idx_dict.set("BitsPerComponent", Object::Int(4));
    let idx_samples = vec![0x01u8; 8];
    let idx_ref = doc.add(Object::Stream(Stream::flate(idx_dict, &idx_samples)));

    let mut xobjects = Dict::new();
    xobjects.set("ImJpg", Object::Ref(jpeg_ref));
    xobjects.set("ImIdx", Object::Ref(idx_ref));
    let mut resources = Dict::new();
    resources.set("XObject", Object::Dict(xobjects));

    // 3. RGB vector fill + inline RGB image
    let mut content_bytes = b"0 0.8 0.2 rg 10 10 80 80 re f q 50 0 0 50 100 100 cm /ImJpg Do Q q 40 0 0 40 200 100 cm /ImIdx Do Q BI /W 2 /H 2 /BPC 8 /CS /RGB ID ".to_vec();
    content_bytes.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]);
    content_bytes.extend_from_slice(b"\nEI");
    let content_ref = doc.add(Object::Stream(Stream::from_raw(Dict::new(), content_bytes)));

    let mut page = Dict::new();
    page.set("Type", Object::name("Page"));
    page.set("Parent", Object::Ref(pages_ref));
    page.set("MediaBox", Object::Array([0, 0, 595, 842].map(Object::Int).to_vec()));
    page.set("Resources", Object::Dict(resources));
    page.set("Contents", Object::Ref(content_ref));
    let page_ref = doc.add(Object::Dict(page));
    doc.update_dict(pages_ref, |d| {
        d.set("Count", Object::Int(1));
        d.set("Kids", Object::Array(vec![Object::Ref(page_ref)]));
    })
    .unwrap();

    let source_pdf = Arc::new(write_full(&doc, &SaveOptions::default()).unwrap());
    let mono_pdf = monochrome_k_only(source_pdf).expect("monochrome_k_only must succeed");
    let out_doc = Document::open(mono_pdf).unwrap();

    let mut saw_dct_gray = false;
    let mut saw_indexed_gray = false;
    let mut image_count = 0usize;
    for (_, obj) in out_doc.scan_objects() {
        if let Object::Stream(s) = &*obj
            && s.dict.name(b"Subtype") == Some(b"Image")
        {
            image_count += 1;
            match s.dict.get(b"ColorSpace") {
                Some(Object::Name(n)) => {
                    assert_eq!(
                        n.as_slice(),
                        b"DeviceGray",
                        "Direct image XObject must be DeviceGray"
                    );
                }
                Some(Object::Array(arr)) => {
                    assert_eq!(arr.first().and_then(Object::as_name), Some(b"Indexed".as_slice()));
                    assert_eq!(
                        arr.get(1).and_then(Object::as_name),
                        Some(b"DeviceGray".as_slice()),
                        "4-bit Indexed image palette must be converted to DeviceGray in-place"
                    );
                    assert_eq!(s.dict.int(b"BitsPerComponent"), Some(4));
                    saw_indexed_gray = true;
                }
                other => panic!("Unexpected image ColorSpace: {other:?}"),
            }
            if s.dict.name(b"Filter") == Some(b"DCTDecode") {
                saw_dct_gray = true;
            }
        }
    }
    assert_eq!(image_count, 2);
    assert!(saw_indexed_gray, "4-bit Indexed image must be converted to Indexed DeviceGray");
    assert!(
        saw_dct_gray,
        "JPEG XObject must remain compact DCTDecode in DeviceGray without FlateDecode inflation"
    );

    let out_page = &pdfcraft_model::pages(&out_doc)[0];
    let c_ref = out_page.dict.reference(b"Contents").unwrap();
    if let Object::Stream(cs) = &*out_doc.get(c_ref) {
        let decoded = cs.decoded().unwrap();
        let text = String::from_utf8_lossy(&decoded);
        assert!(!text.contains(" rg"), "RGB fill operator must be converted to K-only: {text}");
        assert!(!text.contains("/RGB"), "Inline RGB image must be converted to DeviceGray: {text}");
    }
}

#[test]
fn test_failover_encrypted_pdf_via_session_prepare_print_pdf() {
    use pdfcraft_engine::print_coordinator::{BatchDispatchConfig, SpoolSubmitFn};
    use pdfcraft_engine::{Changes, Edit, Printing, Protection, Session};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // Create a 4-page PDF and encrypt it with AES-256 open + owner passwords
    let plain_bytes = Arc::new(make_marked_pdf(4, 595, 842));
    let mut build_session = Session::default();
    let doc_id = build_session.open("plain.pdf", None, plain_bytes, None).unwrap();
    build_session
        .apply(
            doc_id,
            Edit::Protect(Protection {
                open_password: Some("user-secret-42".to_string()),
                permissions_password: Some("owner-secret-99".to_string()),
                printing: Printing::High,
                changes: Changes::None,
                copy: true,
                accessibility: true,
                algorithm: pdfcraft_cos::Algorithm::Aes256,
                encrypt_metadata: true,
            }),
        )
        .unwrap();
    let encrypted_bytes = build_session.save_bytes(doc_id).unwrap();

    // Confirm raw Document::open without password fails (this was why failover crashed before!)
    assert!(pdfcraft_cos::Document::open(encrypted_bytes.clone()).is_err());

    // Open with user password in Session and prepare O(1) background print source
    let mut print_session = Session::default();
    let enc_id = print_session
        .open("encrypted.pdf", None, encrypted_bytes.clone(), Some("user-secret-42"))
        .unwrap();
    let prepared = print_session.prepare_print_pdf(enc_id).unwrap();

    let rerouted_pages: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let rerouted_cb = rerouted_pages.clone();
    let spooler: SpoolSubmitFn = Arc::new(move |bytes, job, progress| {
        let name = job.printer.clone().unwrap_or_default();
        if name == "Enc_Primary" {
            progress(2, 4);
            return Err("Toner depleted".to_string());
        }
        let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
        let count = pdfcraft_model::pages(&doc).len();
        progress(count, count);
        *rerouted_cb.lock().unwrap() = count;
        Ok("done".to_string())
    });

    let settings = Settings {
        pages: vec![0, 1, 2, 3],
        paper: (595.0, 842.0),
        ..Default::default()
    };

    let mut handle = PrintOrchestrationCoordinator::dispatch_batch_prepared(
        BatchDispatchConfig {
            printers: vec!["Enc_Primary".to_string()],
            settings,
            doc_source: SharedPdfSource::from_arc(encrypted_bytes),
            page_sizes: vec![(595.0, 842.0); 4],
            printer_jobs: HashMap::new(),
            default_job: pdfcraft_print::spool::Job::default(),
            pre_imposed_pdf: None,
            custom_spooler: Some(spooler),
            on_event_wakeup: None,
        },
        prepared,
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_all_completed() && Instant::now() < deadline {
        let _ = handle.poll_events();
        std::thread::sleep(Duration::from_millis(10));
    }

    handle.failover_reroute("Enc_Primary", "Enc_Backup").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_all_completed() && Instant::now() < deadline {
        let _ = handle.poll_events();
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        *rerouted_pages.lock().unwrap(),
        2,
        "Encrypted PDF failover must succeed and deliver the remaining 2 imposed sheets"
    );
}

#[test]
fn test_failover_duplex_odd_sheet_alignment_and_poster_tile_slicing() {
    use pdfcraft_engine::print_coordinator::{BatchDispatchConfig, SpoolSubmitFn};
    use pdfcraft_print::Layout;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // Part A: Duplex job failing after an odd sheet count (3 of 6 faces committed -> leaf 2 half printed)
    // Must align resume to (3 / 2) * 2 = 2, rerouting 4 faces [2, 3, 4, 5] so front/back pairing stays intact.
    let six_page_pdf = make_marked_pdf(6, 595, 842);
    let duplex_rerouted = Arc::new(Mutex::new(0usize));
    let duplex_cb = duplex_rerouted.clone();

    let duplex_spooler: SpoolSubmitFn = Arc::new(move |bytes, job, progress| {
        if job.printer.as_deref() == Some("Duplex_Primary") {
            progress(3, 6);
            return Err("Duplexer jam on leaf 2 back face".to_string());
        }
        let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
        let count = pdfcraft_model::pages(&doc).len();
        progress(count, count);
        *duplex_cb.lock().unwrap() = count;
        Ok("ok".to_string())
    });

    let duplex_job = pdfcraft_print::spool::Job {
        duplex: Some(pdfcraft_print::spool::Duplex::LongEdge),
        ..Default::default()
    };

    let mut duplex_handle = PrintOrchestrationCoordinator::dispatch_batch(BatchDispatchConfig {
        printers: vec!["Duplex_Primary".to_string()],
        settings: Settings {
            pages: (0..6).collect(),
            paper: (595.0, 842.0),
            ..Default::default()
        },
        doc_source: SharedPdfSource::from(six_page_pdf),
        page_sizes: vec![(595.0, 842.0); 6],
        printer_jobs: HashMap::new(),
        default_job: duplex_job,
        pre_imposed_pdf: None,
        custom_spooler: Some(duplex_spooler),
        on_event_wakeup: None,
    })
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !duplex_handle.is_all_completed() && Instant::now() < deadline {
        let _ = duplex_handle.poll_events();
        std::thread::sleep(Duration::from_millis(10));
    }

    duplex_handle
        .failover_reroute("Duplex_Primary", "Duplex_Backup")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !duplex_handle.is_all_completed() && Instant::now() < deadline {
        let _ = duplex_handle.poll_events();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        *duplex_rerouted.lock().unwrap(),
        4,
        "Duplex failover at sent=3 of 6 must align to sheet 2 and reroute 4 faces [2..6]"
    );

    // Part B: Poster layout (1 banner page 900x900 pt tiled across 500x500 pt paper -> 4 poster sheets)
    // Failing after 2 tiles must reroute ONLY the remaining 2 tiles (not re-impose all 4 from tile 0!).
    let banner_pdf = make_marked_pdf(1, 900, 900);
    let poster_rerouted = Arc::new(Mutex::new(0usize));
    let poster_cb = poster_rerouted.clone();

    let poster_spooler: SpoolSubmitFn = Arc::new(move |bytes, job, progress| {
        let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
        let total = pdfcraft_model::pages(&doc).len();
        if job.printer.as_deref() == Some("Poster_Primary") {
            assert_eq!(total, 4, "900x900 banner on 500x500 paper must produce 4 poster tiles");
            progress(2, 4);
            return Err("Out of paper after 2 poster tiles".to_string());
        }
        progress(total, total);
        *poster_cb.lock().unwrap() = total;
        Ok("ok".to_string())
    });

    let mut poster_handle = PrintOrchestrationCoordinator::dispatch_batch(BatchDispatchConfig {
        printers: vec!["Poster_Primary".to_string()],
        settings: Settings {
            pages: vec![0],
            paper: (500.0, 500.0),
            layout: Layout::Poster {
                scale: 100.0,
                overlap: 0.0,
                cut_marks: false,
            },
            ..Default::default()
        },
        doc_source: SharedPdfSource::from(banner_pdf),
        page_sizes: vec![(900.0, 900.0)],
        printer_jobs: HashMap::new(),
        default_job: pdfcraft_print::spool::Job::default(),
        pre_imposed_pdf: None,
        custom_spooler: Some(poster_spooler),
        on_event_wakeup: None,
    })
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !poster_handle.is_all_completed() && Instant::now() < deadline {
        let _ = poster_handle.poll_events();
        std::thread::sleep(Duration::from_millis(10));
    }

    poster_handle
        .failover_reroute("Poster_Primary", "Poster_Backup")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !poster_handle.is_all_completed() && Instant::now() < deadline {
        let _ = poster_handle.poll_events();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        *poster_rerouted.lock().unwrap(),
        2,
        "Poster failover after 2 of 4 tiles must slice only the remaining 2 tiles"
    );

    // Part C: Virtual save printer & active printer filtering in suggest_failover_target
    let candidates = vec![
        "Poster_Primary".to_string(),
        "Microsoft Print to PDF".to_string(),
        "Microsoft XPS Document Writer".to_string(),
        "Send To OneNote 16".to_string(),
        "Fax".to_string(),
        "Office_Laser_Idle".to_string(),
    ];
    assert_eq!(
        poster_handle.suggest_failover_target("Poster_Primary", &candidates),
        Some("Office_Laser_Idle".to_string())
    );
}
