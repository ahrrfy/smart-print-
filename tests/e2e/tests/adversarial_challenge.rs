use pdfcraft_e2e::mock_driver::{
    create_mock_virtual_pdf_printer, DevModeValidationError, MockV3Driver,
    MockV3DriverConfig, MockV4Driver, MockV4DriverConfig,
    DEVMODEW_BASE_SIZE, DMCOLOR_MONOCHROME,
    DM_COLOR, DM_DUPLEX, DM_IN_BUFFER, DM_OUT_BUFFER,
};
use pdfcraft_e2e::mock_spooler::{
    CommittedSheetsLedger, DiskVerdict, MockDiskSpaceGovernor,
};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ============================================================================
// PART 1: V3 DEVMODE Buffer Allocation Adversarial Challenges
// ============================================================================

/// Challenge 1.1: What happens with exactly 220 bytes (missing vendor extra)?
/// Tests both out_buf (Pass 2 output) and in_buf (Pass 2 input).
#[test]
fn challenge_v3_devmode_exactly_220_bytes_rejected() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let required = mock.required_buffer_size();
    assert_eq!(required, 220 + 3704); // 3924 bytes total

    // 1. Output buffer has exactly DEVMODEW_BASE_SIZE (220 bytes), missing 3704 vendor extra
    let mut exact_220_out = vec![0u8; DEVMODEW_BASE_SIZE];
    let res_out = mock.document_properties_pass2(None, &mut exact_220_out, DM_OUT_BUFFER);
    assert_eq!(
        res_out,
        Err(DevModeValidationError::BufferTooSmall {
            provided: 220,
            required: 3924,
        }),
        "Expected output buffer of exactly 220 bytes to be rejected with BufferTooSmall"
    );

    // 2. Input buffer has exactly DEVMODEW_BASE_SIZE (220 bytes), while output buffer is 3924
    let mut valid_out = vec![0u8; required];
    let exact_220_in = vec![0u8; DEVMODEW_BASE_SIZE];
    let res_in = mock.document_properties_pass2(
        Some(&exact_220_in),
        &mut valid_out,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert_eq!(
        res_in,
        Err(DevModeValidationError::BufferTooSmall {
            provided: 220,
            required: 3924,
        }),
        "Expected input buffer of exactly 220 bytes to be rejected with BufferTooSmall"
    );
}

/// Challenge 1.2: What happens when canary byte 3703 of 3704 is corrupted?
/// The vendor extra is 3704 bytes (indices 0..3703 relative to offset 220).
/// Index 3703 is the very last byte of the vendor payload (offset 220 + 3703 = 3923).
#[test]
fn challenge_v3_devmode_canary_byte_3703_of_3704_corrupted() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let required = mock.required_buffer_size();
    assert_eq!(required, 3924);

    let mut devmode_buf = vec![0u8; required];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Failed to initialize default DEVMODE buffer");

    // Corrupt the very last canary byte: index 3703 (offset 220 + 3703 = 3923)
    let last_canary_offset = DEVMODEW_BASE_SIZE + 3703;
    assert_eq!(last_canary_offset, 3923);
    devmode_buf[last_canary_offset] ^= 0xAA;

    let mut out_buf = vec![0u8; required];
    let res = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );

    assert_eq!(
        res,
        Err(DevModeValidationError::CorruptedVendorExtraPayload {
            expected_len: 3704,
            actual_len: 3704,
        }),
        "Corrupting canary byte 3703 of 3704 MUST trigger CorruptedVendorExtraPayload"
    );

    // Also verify corrupting canary byte 0 (offset 220) triggers the same
    devmode_buf[last_canary_offset] ^= 0xAA; // Restore byte 3703
    devmode_buf[DEVMODEW_BASE_SIZE] ^= 0x55; // Corrupt byte 0
    let res_byte0 = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert_eq!(
        res_byte0,
        Err(DevModeValidationError::CorruptedVendorExtraPayload {
            expected_len: 3704,
            actual_len: 3704,
        }),
        "Corrupting canary byte 0 of 3704 MUST trigger CorruptedVendorExtraPayload"
    );
}

/// Challenge 1.3: What happens when dmFields bit is set for Duplex but dmDuplex has an invalid enum value (e.g. 99)?
#[test]
fn challenge_v3_devmode_dmfields_duplex_with_invalid_enum() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let required = mock.required_buffer_size();

    let mut devmode_buf = vec![0u8; required];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .unwrap();

    let mut out_buf = vec![0u8; required];

    // Case A: dmFields bit DM_DUPLEX is set, but dmDuplex is invalid enum value 99
    let dm_fields = DM_DUPLEX;
    devmode_buf[72..76].copy_from_slice(&dm_fields.to_le_bytes());
    let invalid_duplex: i16 = 99;
    devmode_buf[94..96].copy_from_slice(&invalid_duplex.to_le_bytes());

    let res = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert_eq!(
        res,
        Err(DevModeValidationError::InvalidDuplexValue(99)),
        "When DM_DUPLEX bit is set, invalid enum value 99 MUST be rejected with InvalidDuplexValue(99)"
    );

    // Case B: dmDuplex is 99, BUT dmFields DM_DUPLEX bit is NOT set (e.g. only DM_COLOR is set)
    let dm_fields_no_duplex = DM_COLOR;
    devmode_buf[72..76].copy_from_slice(&dm_fields_no_duplex.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());
    // devmode_buf[94..96] is still 99!
    let res_ignored = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert!(
        res_ignored.is_ok(),
        "When DM_DUPLEX bit is NOT set, invalid value in dmDuplex field MUST be ignored per Win32 GDI contract"
    );
    let captured = mock.last_submitted_devmode().unwrap();
    assert_eq!(
        captured.dm_duplex, None,
        "Driver should not capture dm_duplex when DM_DUPLEX is not in dm_fields"
    );
    assert_eq!(captured.dm_color, Some(DMCOLOR_MONOCHROME));

    // Case C: dmFields DM_COLOR is set, but dmColor is 99
    let dm_fields_color = DM_COLOR;
    devmode_buf[72..76].copy_from_slice(&dm_fields_color.to_le_bytes());
    let invalid_color: i16 = 99;
    devmode_buf[92..94].copy_from_slice(&invalid_color.to_le_bytes());
    let res_color = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert_eq!(
        res_color,
        Err(DevModeValidationError::InvalidColorValue(99)),
        "When DM_COLOR bit is set, invalid color value 99 MUST be rejected with InvalidColorValue(99)"
    );
}

// ============================================================================
// PART 2: V4 PrintTicket Simulation Adversarial Challenges
// ============================================================================

/// Challenge 2.1: What happens when PrintTicket XML has malformed XML syntax or custom namespaces?
#[test]
fn challenge_v4_printticket_malformed_xml_and_custom_namespaces() {
    let mock = MockV4Driver::new(MockV4DriverConfig::default());

    // Scenario A: Malformed, non-XML syntax passed to pt_merge_and_validate
    let malformed_xml = "<<<MALFORMED PRINT TICKET>>> <unclosed tag> !!! NOT XML ???";
    let res_malformed = mock.pt_merge_and_validate(None, malformed_xml);
    // Observe behavior: pt_merge_and_validate does substring searching rather than XML parsing
    assert!(
        res_malformed.is_ok(),
        "pt_merge_and_validate accepts arbitrary text because it relies on substring searching (.contains)"
    );
    let merged_malformed = res_malformed.unwrap();
    // Default fallback values are used because "psk:Monochrome", etc. are not found
    assert!(merged_malformed.contains("name=\"psk:Color\""));
    assert!(merged_malformed.contains("name=\"psk:OneSided\""));

    // Scenario B: Custom namespace prefix (e.g. xmlns:custom instead of xmlns:psk)
    let custom_ns_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework"
                 xmlns:custom="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="custom:PageOutputColor">
        <psf:Option name="custom:Monochrome"/>
    </psf:Feature>
    <psf:Feature name="custom:JobDuplexAllDocumentsContiguously">
        <psf:Option name="custom:TwoSidedLongEdge"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let res_custom = mock.pt_merge_and_validate(None, custom_ns_xml).unwrap();
    // Because mock checks delta_ticket_xml.contains("psk:Monochrome"), it fails to detect "custom:Monochrome"!
    assert!(
        res_custom.contains("name=\"psk:Color\""),
        "Mock driver substring search fails to recognize custom namespace prefix 'custom:Monochrome', defaulting to Color"
    );
    assert!(
        res_custom.contains("name=\"psk:OneSided\""),
        "Mock driver substring search fails to recognize custom namespace prefix 'custom:TwoSidedLongEdge', defaulting to OneSided"
    );

    // Scenario C: Commented-out feature in XML
    let commented_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework"
                 xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <!-- User previously tested monochrome: <psf:Option name="psk:Monochrome"/> -->
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Color"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let res_commented = mock.pt_merge_and_validate(None, commented_xml).unwrap();
    // Because .contains("psk:Monochrome") is true even in comments, it mistakenly chooses Monochrome!
    assert!(
        res_commented.contains("name=\"psk:Monochrome\""),
        "Mock driver's naive string contains catches commented-out options, revealing an XML fidelity limitation"
    );
}

/// Challenge 2.2: What happens when requesting duplex and monochrome simultaneously on a virtual simplex color printer?
/// Does conflict resolution resolve cleanly?
#[test]
fn challenge_v4_simultaneous_duplex_and_mono_on_virtual_simplex_color_printer() {
    let mock_virtual = create_mock_virtual_pdf_printer("Virtual_Simplex_Color_PDF");
    assert!(mock_virtual.config.supports_color);
    assert!(!mock_virtual.config.supports_monochrome);
    assert!(!mock_virtual.config.supports_duplex);

    // Request BOTH monochrome AND duplex long edge simultaneously
    let delta_ticket = r#"<?xml version="1.0" encoding="UTF-8"?>
<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework"
                 xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords"
                 version="1">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="psk:TwoSidedLongEdge"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged_xml = mock_virtual
        .pt_merge_and_validate(None, delta_ticket)
        .expect("pt_merge_and_validate should succeed and resolve conflicts");

    // 1. Verify merged XML has resolved both conflicting features cleanly:
    assert!(
        merged_xml.contains(r#"<psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Color"/>"#),
        "Monochrome request MUST resolve to Color on virtual color printer"
    );
    assert!(
        merged_xml.contains(r#"<psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="psk:OneSided"/>"#),
        "Duplex request MUST resolve to OneSided on virtual simplex printer"
    );

    // 2. Verify both conflicts are explicitly recorded with reasons in conflicts ledger:
    let conflicts = mock_virtual.conflicts();
    assert_eq!(conflicts.len(), 2, "Expected exactly 2 conflicts recorded");

    let color_conflict = conflicts
        .iter()
        .find(|c| c.feature == "psk:PageOutputColor")
        .expect("Missing PageOutputColor conflict");
    assert_eq!(color_conflict.requested_option, "psk:Monochrome");
    assert_eq!(color_conflict.resolved_option, "psk:Color");
    assert!(color_conflict.reason.contains("omits monochrome"));

    let duplex_conflict = conflicts
        .iter()
        .find(|c| c.feature == "psk:JobDuplexAllDocumentsContiguously")
        .expect("Missing JobDuplexAllDocumentsContiguously conflict");
    assert_eq!(duplex_conflict.requested_option, "psk:TwoSidedLongEdge");
    assert_eq!(duplex_conflict.resolved_option, "psk:OneSided");
    assert!(duplex_conflict.reason.contains("does not support duplex"));
}

/// Challenge 2.3: What happens when concurrent calls invoke pt_open_provider without PRINT_TICKET_PROVIDER lock?
/// Does assert_no_concurrency_violations correctly detect it?
#[test]
fn challenge_v4_concurrent_pt_open_provider_without_lock_detected() {
    let mock = Arc::new(MockV4Driver::new(MockV4DriverConfig::default()));

    // Phase 1: Unlocked concurrent calls -> MUST trigger concurrency violations
    let mut handles = Vec::new();
    for _ in 0..8 {
        let m = Arc::clone(&mock);
        handles.push(thread::spawn(move || {
            let _ = m.pt_open_provider();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Verify assert_no_concurrency_violations panics with diagnostic message
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mock.assert_no_concurrency_violations();
    }));
    assert!(
        panic_result.is_err(),
        "assert_no_concurrency_violations MUST panic when concurrency violations occurred without lock"
    );
    assert!(
        panic_result.is_err(),
        "assert_no_concurrency_violations MUST panic when concurrency violations > 0"
    );

    // Phase 2: Serialized calls with PRINT_TICKET_PROVIDER lock simulation -> 0 violations
    let clean_mock = Arc::new(MockV4Driver::new(MockV4DriverConfig::default()));
    let provider_lock = Arc::new(Mutex::new(()));
    let mut clean_handles = Vec::new();

    for _ in 0..8 {
        let m = Arc::clone(&clean_mock);
        let lock = Arc::clone(&provider_lock);
        clean_handles.push(thread::spawn(move || {
            let _guard = lock.lock().unwrap();
            let _ = m.pt_open_provider();
        }));
    }
    for h in clean_handles {
        h.join().unwrap();
    }

    // When serialized with lock, violations MUST remain 0 and assert succeeds
    clean_mock.assert_no_concurrency_violations();
}

// ============================================================================
// PART 3: Virtual Spooler Adversarial Challenges
// ============================================================================

/// Challenge 3.1: Controllable disk space governor: exact boundary where available == estimated_bytes.
/// Is it Sufficient or LowSpaceWarning or CriticalExhaustion?
#[test]
fn challenge_disk_space_governor_exact_boundaries() {
    let governor = MockDiskSpaceGovernor::new(0);
    let spool_dir = Path::new("C:\\fake\\spool");
    let estimated: u64 = 200 * 1024 * 1024; // 200 MB

    // Boundary 1: available == estimated - 1 byte
    governor.set_available_bytes(estimated - 1);
    let status_under = governor.evaluate(spool_dir, estimated);
    assert!(
        matches!(status_under.verdict, DiskVerdict::CriticalExhaustion { .. }),
        "When available < estimated, verdict MUST be CriticalExhaustion. Got {:?}",
        status_under.verdict
    );

    // Boundary 2: EXACT BOUNDARY available == estimated_bytes
    governor.set_available_bytes(estimated);
    let status_exact = governor.evaluate(spool_dir, estimated);
    assert_eq!(
        status_exact.verdict,
        DiskVerdict::LowSpaceWarning {
            available_mb: 200,
            estimated_mb: 200,
        },
        "When available == estimated_bytes, verdict is LowSpaceWarning (avail < 1.25 * estimated)"
    );

    // Boundary 3: available == estimated + 1 byte
    governor.set_available_bytes(estimated + 1);
    let status_over1 = governor.evaluate(spool_dir, estimated);
    assert!(
        matches!(status_over1.verdict, DiskVerdict::LowSpaceWarning { .. }),
        "When available == estimated + 1, verdict MUST still be LowSpaceWarning"
    );

    // Boundary 4: available == 1.25 * estimated - 1 byte (250MB - 1)
    let threshold_125 = (estimated as f64 * 1.25) as u64; // 250 MB
    governor.set_available_bytes(threshold_125 - 1);
    let status_under_125 = governor.evaluate(spool_dir, estimated);
    assert!(
        matches!(status_under_125.verdict, DiskVerdict::LowSpaceWarning { .. }),
        "When available < 1.25 * estimated, verdict MUST be LowSpaceWarning"
    );

    // Boundary 5: available == 1.25 * estimated bytes (250MB) -> Sufficient
    governor.set_available_bytes(threshold_125);
    let status_at_125 = governor.evaluate(spool_dir, estimated);
    assert_eq!(
        status_at_125.verdict,
        DiskVerdict::Sufficient,
        "When available >= 1.25 * estimated (and >= 100MB), verdict MUST be Sufficient"
    );

    // Boundary 6: Small job (10MB) where 1.25 * estimated = 12.5MB, but available is 50MB (< 100MB floor)
    let small_estimated: u64 = 10 * 1024 * 1024;
    governor.set_available_bytes(50 * 1024 * 1024); // 50MB is > 1.25 * 10MB, but < 100MB
    let status_small = governor.evaluate(spool_dir, small_estimated);
    assert_eq!(
        status_small.verdict,
        DiskVerdict::LowSpaceWarning {
            available_mb: 50,
            estimated_mb: 10,
        },
        "When available < 100MB, verdict MUST be LowSpaceWarning regardless of estimated ratio"
    );
}

/// Challenge 3.2: CommittedSheetsLedger stress test with 1,000 sheets.
/// Verifies concurrency, backpressure enforcement, and permit reclamation.
#[test]
fn challenge_committed_sheets_ledger_1000_sheets_stress_and_permit_reclamation() {
    const TOTAL_SHEETS: usize = 1000;
    const MAX_IN_FLIGHT: usize = 2;
    const NUM_WORKERS: usize = 8;

    let ledger = Arc::new(CommittedSheetsLedger::new(TOTAL_SHEETS, MAX_IN_FLIGHT));
    let next_sheet_counter = Arc::new(AtomicUsize::new(0));
    let rejected_count = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for worker_id in 0..NUM_WORKERS {
        let ledger_cloned = Arc::clone(&ledger);
        let counter = Arc::clone(&next_sheet_counter);
        let rejects = Arc::clone(&rejected_count);

        handles.push(thread::spawn(move || {
            loop {
                // Fetch next sheet to process
                let sheet_idx = counter.fetch_add(1, Ordering::SeqCst);
                if sheet_idx >= TOTAL_SHEETS {
                    break;
                }

                // Attempt to begin sheet with backpressure retry loop
                let mut retry_count = 0;
                loop {
                    match ledger_cloned.begin_sheet(sheet_idx, worker_id) {
                        Ok(()) => {
                            // Sheet successfully in-flight!
                            // Simulate small rendering/spooling workload (10 microseconds)
                            std::thread::sleep(Duration::from_micros(10));

                            // Commit sheet
                            ledger_cloned.commit_sheet(sheet_idx, 1024);
                            break;
                        }
                        Err(_backpressure_violation) => {
                            // Backpressure violation hit!
                            rejects.fetch_add(1, Ordering::SeqCst);
                            retry_count += 1;
                            // Backoff briefly to allow in-flight sheets to commit
                            std::thread::yield_now();
                            if retry_count % 5 == 0 {
                                std::thread::sleep(Duration::from_micros(50));
                            }
                        }
                    }
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Assertions:
    // 1. All 1,000 sheets committed
    assert_eq!(
        ledger.committed_count(),
        TOTAL_SHEETS,
        "All 1,000 sheets MUST be committed successfully"
    );
    assert!(
        ledger.is_all_committed(),
        "is_all_committed() MUST return true for 1,000 sheets"
    );

    // 2. Strict permit reclamation: current_in_flight MUST return to 0
    let remaining_permits = ledger.current_in_flight();
    assert_eq!(
        remaining_permits, 0,
        "Permit leak detected! current_in_flight should be 0 after 1,000 sheets committed, got {remaining_permits}"
    );

    // 3. Verify zero sheets pending for failover
    let pending = ledger.pending_sheets_for_failover();
    assert!(
        pending.is_empty(),
        "No sheets should be pending for failover after 100% commit, found {:?}",
        pending
    );

    // 4. Inspect peak_in_flight_observed vs backpressure rejections:
    let peak = ledger.peak_in_flight_observed();
    let rejects = rejected_count.load(Ordering::SeqCst);
    println!("Ledger stress completed: committed=1000, rejects={rejects}, peak_in_flight={peak}");

    // Note critical architectural insight:
    // When 8 threads call begin_sheet concurrently without an external permit gate,
    // in_flight counter increments momentarily before being checked and decremented.
    // If rejects > 0, peak_in_flight recorded the momentary in_flight count.
    if rejects > 0 {
        assert!(
            peak >= MAX_IN_FLIGHT,
            "When rejections occur, peak_in_flight records concurrent caller attempts"
        );
    }
}

/// Challenge 3.2b: CommittedSheetsLedger failure permit reclamation and failover slicing.
/// Verifies that fail_sheet also decrements in_flight and pending_sheets_for_failover is exact.
#[test]
fn challenge_committed_sheets_ledger_fail_sheet_permit_reclamation() {
    let ledger = CommittedSheetsLedger::new(10, 2);

    // Begin sheet 0, 1
    ledger.begin_sheet(0, 1).unwrap();
    ledger.begin_sheet(1, 1).unwrap();
    assert_eq!(ledger.current_in_flight(), 2);

    // Sheet 0 commits
    ledger.commit_sheet(0, 500);
    assert_eq!(ledger.current_in_flight(), 1);

    // Sheet 1 fails (e.g. paper jam)
    ledger.fail_sheet(1, "Paper Jam at sheet 1".into());
    assert_eq!(
        ledger.current_in_flight(),
        0,
        "fail_sheet MUST reclaim permit, returning current_in_flight to 0"
    );

    // Slicing for failover must return sheet 1, plus unstarted sheets 2..10
    let pending = ledger.pending_sheets_for_failover();
    assert_eq!(
        pending,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
        "Failover pending sheets must include failed sheet 1 and unstarted sheets 2..9"
    );
}
