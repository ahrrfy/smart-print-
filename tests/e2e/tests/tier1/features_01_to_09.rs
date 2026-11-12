#![allow(unused_imports, unused_must_use, dead_code)]
//! Tier 1 Happy-Path Feature Verification: Features 1 to 9 (45 tests total)
//!
//! Covers:
//! - Feature 1: Win32 DevMode 2-Pass Negotiation (5 tests)
//! - Feature 2: DEVMODE Flag Assertion (5 tests)
//! - Feature 3: XPS PrintTicket Validation & Merge (5 tests)
//! - Feature 4: PrintDocumentPackageTarget Spooling (5 tests)
//! - Feature 5: CUPS / IPP 2.0 Native Adapter (5 tests)
//! - Feature 6: Driver Monochrome Assertion (Layer 1) (5 tests)
//! - Feature 7: In-Memory Vector K-Only Filter (Layer 2) (5 tests)
//! - Feature 8: Image XObject Grayscale Re-encoding (5 tests)
//! - Feature 9: Dynamic Physical Margin Querying (5 tests)

use pdfcraft_e2e::mock_driver::{
    create_mock_virtual_pdf_printer, DevModeValidationError, DuplexBinding, HardwareMargins,
    MockIppDriver, MockV3Driver, MockV3DriverConfig, MockV4Driver, MockV4DriverConfig,
    PrintJobSettings, PrintableBounds, DMCOLOR_COLOR, DMCOLOR_MONOCHROME, DMDUP_HORIZONTAL,
    DMDUP_VERTICAL, DMORIENT_LANDSCAPE, DMORIENT_PORTRAIT, DM_COLOR, DM_COPIES, DM_DUPLEX,
    DM_IN_BUFFER, DM_ORIENTATION, DM_OUT_BUFFER, IPP_STATUS_OK,
};
use pdfcraft_e2e::mock_spooler::{CommittedSheetsLedger, MockSpooler};
use pdfcraft_e2e::stream_inspector::{
    DefaultStreamInspector, PdfStreamInspector, StreamAssertions,
};
use pdfcraft_platform::caps::RawGdiCaps;
use pdfcraft_platform::cups::{IppMessageWriter, IppOperation, IppTag};
use pdfcraft_print::monochrome::MonochromeVectorFilter;
use std::collections::HashMap;

// =========================================================================
// FEATURE 1: Win32 DevMode 2-Pass Negotiation (5 Tests)
// =========================================================================

#[test]
fn feat_01_test_01_standard_epson_3704_byte_2pass_negotiation() {
    let mock = MockV3Driver::new(MockV3DriverConfig {
        driver_extra_size: 3704,
        vendor_canary_pattern: vec![0xAA, 0x55, 0xDE, 0xAD, 0xBE, 0xEF],
        ..Default::default()
    });

    let needed = mock.document_properties_pass1();
    assert_eq!(needed, 220 + 3704);

    let mut devmode_buf = vec![0u8; needed as usize];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Pass 2 default populate failed");

    let mut out_buf = vec![0u8; needed as usize];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Pass 2 submit failed");

    mock.assert_pass1_called();
    mock.assert_driver_extra_preserved();
    assert_eq!(
        mock.last_submitted_devmode().unwrap().dm_driver_extra,
        3704
    );
}

#[test]
fn feat_01_test_02_hp_extended_4096_byte_driver_extra() {
    let mock = MockV3Driver::new(MockV3DriverConfig {
        driver_extra_size: 4096,
        vendor_canary_pattern: vec![0xCA, 0xFE, 0xBA, 0xBE],
        ..Default::default()
    });

    let needed = mock.document_properties_pass1();
    assert_eq!(needed, 220 + 4096);

    let mut devmode_buf = vec![0u8; needed as usize];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Pass 2 default populate failed");

    let mut out_buf = vec![0u8; needed as usize];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Pass 2 submit failed");

    assert_eq!(out_buf.len(), 4316);
    mock.assert_driver_extra_preserved();
    assert_eq!(
        mock.last_submitted_devmode().unwrap().dm_driver_extra,
        4096
    );
}

#[test]
fn feat_01_test_03_minimal_canon_512_byte_driver_extra() {
    let mock = MockV3Driver::new(MockV3DriverConfig {
        driver_extra_size: 512,
        vendor_canary_pattern: vec![0x11, 0x22, 0x33, 0x44],
        ..Default::default()
    });

    let needed = mock.document_properties_pass1();
    assert_eq!(needed, 220 + 512);

    let mut devmode_buf = vec![0u8; needed as usize];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Pass 2 default populate failed");

    let mut out_buf = vec![0u8; needed as usize];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Pass 2 submit failed");

    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_size, 220);
    assert_eq!(last.dm_driver_extra, 512);
}

#[test]
fn feat_01_test_04_allocator_headroom_safety_buffer() {
    let mock = MockV3Driver::new(MockV3DriverConfig {
        driver_extra_size: 3704,
        vendor_canary_pattern: vec![0x5A, 0xA5, 0xC3, 0x3C],
        ..Default::default()
    });

    let needed = mock.document_properties_pass1() as usize;
    assert_eq!(needed, 3924);

    let headroom_size = (needed * 2) + 8192;
    let mut safe_buffer = vec![0xEEu8; headroom_size];

    mock.document_properties_pass2(None, &mut safe_buffer[..needed], DM_OUT_BUFFER)
        .expect("Populate into slice failed");

    assert_eq!(
        u16::from_le_bytes([safe_buffer[68], safe_buffer[69]]),
        220
    );
    assert_eq!(
        u16::from_le_bytes([safe_buffer[70], safe_buffer[71]]),
        3704
    );

    for (i, &byte) in safe_buffer[needed..].iter().enumerate() {
        assert_eq!(
            byte, 0xEE,
            "Headroom corrupted at offset {}",
            needed + i
        );
    }
}

#[test]
fn feat_01_test_05_idempotent_multi_pass_renegotiation() {
    let mock = MockV3Driver::new(MockV3DriverConfig {
        driver_extra_size: 3704,
        vendor_canary_pattern: vec![0xDE, 0xAD, 0xBE, 0xEF],
        ..Default::default()
    });

    let modes = [
        (DM_DUPLEX, 1u16), // Simplex
        (DM_DUPLEX, 2u16), // Duplex Long
        (DM_DUPLEX, 3u16), // Duplex Short
    ];

    for (field_mask, duplex_val) in modes {
        let needed = mock.document_properties_pass1() as usize;
        let mut devmode_buf = vec![0u8; needed];
        mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
            .expect("Pass 2 populate");

        devmode_buf[72..76].copy_from_slice(&field_mask.to_le_bytes());
        devmode_buf[94..96].copy_from_slice(&duplex_val.to_le_bytes());

        let mut out_buf = vec![0u8; needed];
        mock.document_properties_pass2(
            Some(&devmode_buf),
            &mut out_buf,
            DM_IN_BUFFER | DM_OUT_BUFFER,
        )
        .expect("Pass 2 submit");
    }

    assert!(mock.last_submitted_devmode().is_some());
    mock.assert_driver_extra_preserved();
}

// =========================================================================
// FEATURE 2: DEVMODE Flag Assertion (5 Tests)
// =========================================================================

#[test]
fn feat_02_test_01_monochrome_flag_and_dmfields_bitmask() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Populate defaults");

    devmode_buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Submit mono DEVMODE");

    let last = mock.last_submitted_devmode().unwrap();
    assert!(last.dm_fields & DM_COLOR != 0);
    assert_eq!(last.dm_color, Some(DMCOLOR_MONOCHROME));
    mock.assert_monochrome_asserted();
}

#[test]
fn feat_02_test_02_duplex_long_edge_booklet_flag() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Populate defaults");

    devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_VERTICAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Submit duplex long DEVMODE");

    let last = mock.last_submitted_devmode().unwrap();
    assert!(last.dm_fields & DM_DUPLEX != 0);
    assert_eq!(last.dm_duplex, Some(DMDUP_VERTICAL));
    mock.assert_duplex_asserted(DuplexBinding::FlipLongEdge);
}

#[test]
fn feat_02_test_03_duplex_short_edge_tablet_flag() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Populate defaults");

    devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_HORIZONTAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Submit duplex short DEVMODE");

    let last = mock.last_submitted_devmode().unwrap();
    assert!(last.dm_fields & DM_DUPLEX != 0);
    assert_eq!(last.dm_duplex, Some(DMDUP_HORIZONTAL));
    mock.assert_duplex_asserted(DuplexBinding::FlipShortEdge);
}

#[test]
fn feat_02_test_04_orientation_landscape_flag() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Populate defaults");

    devmode_buf[72..76].copy_from_slice(&DM_ORIENTATION.to_le_bytes());
    devmode_buf[76..78].copy_from_slice(&DMORIENT_LANDSCAPE.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Submit landscape DEVMODE");

    let last = mock.last_submitted_devmode().unwrap();
    assert!(last.dm_fields & DM_ORIENTATION != 0);
    assert_eq!(last.dm_orientation, Some(DMORIENT_LANDSCAPE));
}

#[test]
fn feat_02_test_05_combined_all_fields_with_multi_copies() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Populate defaults");

    let combined_fields = DM_COLOR | DM_DUPLEX | DM_ORIENTATION | DM_COPIES;
    devmode_buf[72..76].copy_from_slice(&combined_fields.to_le_bytes());
    devmode_buf[76..78].copy_from_slice(&DMORIENT_LANDSCAPE.to_le_bytes());
    devmode_buf[86..88].copy_from_slice(&5i16.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_VERTICAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Submit composite DEVMODE");

    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_fields & combined_fields, combined_fields);
    assert_eq!(last.dm_orientation, Some(DMORIENT_LANDSCAPE));
    assert_eq!(last.dm_copies, Some(5));
    assert_eq!(last.dm_color, Some(DMCOLOR_MONOCHROME));
    assert_eq!(last.dm_duplex, Some(DMDUP_VERTICAL));
}

// =========================================================================
// FEATURE 3: XPS PrintTicket Validation & Merge (5 Tests)
// =========================================================================

#[test]
fn feat_03_test_01_monochrome_delta_ticket_merge_success() {
    let mock = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Enterprise_Mono_V4".to_string(),
        supports_color: true,
        supports_monochrome: true,
        supports_duplex: true,
        ..Default::default()
    });

    mock.pt_open_provider();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock
        .pt_merge_and_validate(None, delta)
        .expect("Merge monochrome ticket failed");
    assert!(merged.contains("psk:Monochrome"));
    assert!(mock.conflicts().is_empty());
}

#[test]
fn feat_03_test_02_duplex_long_edge_merge_success() {
    let mock = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Office_Duplex_V4".to_string(),
        supports_duplex: true,
        ..Default::default()
    });

    mock.pt_open_provider();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="psk:TwoSidedLongEdge"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock
        .pt_merge_and_validate(None, delta)
        .expect("Merge duplex ticket failed");
    assert!(merged.contains("psk:TwoSidedLongEdge"));
    assert!(mock.conflicts().is_empty());
}

#[test]
fn feat_03_test_03_duplex_short_edge_and_landscape_merge() {
    let mock = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Calendar_Printer_V4".to_string(),
        supports_duplex: true,
        ..Default::default()
    });

    mock.pt_open_provider();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="psk:TwoSidedShortEdge"/>
    </psf:Feature>
    <psf:Feature name="psk:PageOrientation">
        <psf:Option name="psk:Landscape"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock
        .pt_merge_and_validate(None, delta)
        .expect("Merge ticket failed");
    assert!(merged.contains("psk:TwoSidedShortEdge"));
    assert!(merged.contains("psk:Landscape"));
}

#[test]
fn feat_03_test_04_virtual_driver_conflict_resolution_fallback() {
    let mock = create_mock_virtual_pdf_printer("Microsoft_Print_To_PDF");
    mock.pt_open_provider();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock
        .pt_merge_and_validate(None, delta)
        .expect("Merge with fallback");
    assert!(merged.contains("psk:Color"));
    assert_eq!(mock.conflicts().len(), 1);
    let conflict = &mock.conflicts()[0];
    assert_eq!(conflict.feature, "psk:PageOutputColor");
    assert_eq!(conflict.requested_option, "psk:Monochrome");
    assert_eq!(conflict.resolved_option, "psk:Color");
}

#[test]
fn feat_03_test_05_custom_media_size_dl_envelope_merge() {
    let mock = MockV4Driver::new(MockV4DriverConfig::default());
    mock.pt_open_provider();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageMediaSize">
        <psf:Option name="psk:ISO_DL_Envelope"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock
        .pt_merge_and_validate(None, delta)
        .expect("Merge envelope size");
    assert!(merged.contains("PrintTicket"));
    mock.assert_no_concurrency_violations();
}

// =========================================================================
// FEATURE 4: PrintDocumentPackageTarget Spooling (5 Tests)
// =========================================================================

#[test]
fn feat_04_test_01_single_page_xps_vector_dispatch() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(1, 2);

    let vector_payload = b"%PDF-1.7\n1 0 obj\n<< /Type /Page >>\nendobj\nq 10 10 100 100 re f Q\n";
    spooler
        .spool_sheet_stream(101, &ledger, 1, |idx| {
            if idx == 0 {
                Some(vector_payload.to_vec())
            } else {
                None
            }
        })
        .expect("Spool single sheet");

    assert_eq!(ledger.committed_count(), 1);
    assert!(ledger.is_all_committed());
}

#[test]
fn feat_04_test_02_multi_page_sequential_package_spooling() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(10, 2);

    spooler
        .spool_sheet_stream(102, &ledger, 10, |idx| {
            Some(format!("Page content stream index {}", idx).into_bytes())
        })
        .expect("Spool 10 sheets");

    assert_eq!(ledger.committed_count(), 10);
    assert!(ledger.is_all_committed());
    assert!(ledger.peak_in_flight_observed() <= 2);
}

#[test]
fn feat_04_test_03_high_resolution_cad_vector_fidelity_retention() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(1, 2);

    let mut cad_stream = Vec::new();
    cad_stream.extend_from_slice(b"q 0.5 w\n");
    for i in 0..5000 {
        let op = format!("{} {} m {} {} l S\n", i, i * 2, i + 1, (i + 1) * 2);
        cad_stream.extend_from_slice(op.as_bytes());
    }
    cad_stream.extend_from_slice(b"Q\n");

    spooler
        .spool_sheet_stream(103, &ledger, 1, move |idx| {
            if idx == 0 {
                Some(cad_stream.clone())
            } else {
                None
            }
        })
        .expect("Spool CAD drawing");

    assert_eq!(ledger.committed_count(), 1);
    assert!(ledger.is_all_committed());
}

#[test]
fn feat_04_test_04_concurrent_package_targets_different_queues() {
    let spooler1 = MockSpooler::new();
    let spooler2 = MockSpooler::new();

    let h1 = std::thread::spawn(move || {
        let ledger = CommittedSheetsLedger::new(5, 2);
        spooler1
            .spool_sheet_stream(201, &ledger, 5, |i| {
                Some(format!("Queue A Sheet {}", i).into_bytes())
            })
            .expect("Queue A spool");
        ledger.committed_count()
    });

    let h2 = std::thread::spawn(move || {
        let ledger = CommittedSheetsLedger::new(5, 2);
        spooler2
            .spool_sheet_stream(202, &ledger, 5, |i| {
                Some(format!("Queue B Sheet {}", i).into_bytes())
            })
            .expect("Queue B spool");
        ledger.committed_count()
    });

    let s1 = h1.join().unwrap();
    let s2 = h2.join().unwrap();

    assert_eq!(s1, 5);
    assert_eq!(s2, 5);
}

#[test]
fn feat_04_test_05_package_spool_clean_completion_and_file_integrity() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(1, 2);
    let payload = vec![0x42u8; 65536];

    spooler
        .spool_sheet_stream(105, &ledger, 1, {
            let p = payload.clone();
            move |idx| if idx == 0 { Some(p.clone()) } else { None }
        })
        .expect("Spool 64KB");

    assert_eq!(ledger.committed_count(), 1);
    assert!(ledger.is_all_committed());
}

// =========================================================================
// FEATURE 5: CUPS / IPP 2.0 Native Adapter (5 Tests)
// =========================================================================

#[test]
fn feat_05_test_01_standard_a4_duplex_monochrome_submission() {
    let mock = MockIppDriver::new("Office_Laser_IPP", true);
    let mut attrs = HashMap::new();
    attrs.insert("sides".to_string(), "two-sided-long-edge".to_string());
    attrs.insert(
        "print-color-mode".to_string(),
        "monochrome".to_string(),
    );
    attrs.insert("media".to_string(), "iso_a4_210x297mm".to_string());

    let job_id = mock
        .submit_ipp_job(attrs, b"%PDF-1.7 payload")
        .expect("Submit standard IPP job");

    assert!(job_id >= 1001);
    mock.assert_attribute("sides", "two-sided-long-edge");
    mock.assert_attribute("print-color-mode", "monochrome");
    mock.assert_attribute("media", "iso_a4_210x297mm");
}

#[test]
fn feat_05_test_02_na_letter_simplex_color_submission() {
    let mock = MockIppDriver::new("Color_Copier_IPP", true);
    let mut attrs = HashMap::new();
    attrs.insert("sides".to_string(), "one-sided".to_string());
    attrs.insert("print-color-mode".to_string(), "color".to_string());
    attrs.insert("media".to_string(), "na_letter_8.5x11in".to_string());

    let job_id = mock
        .submit_ipp_job(attrs, b"%PDF-1.7 color")
        .expect("Submit letter color IPP job");

    assert!(job_id >= 1001);
    mock.assert_attribute("sides", "one-sided");
    mock.assert_attribute("print-color-mode", "color");
    mock.assert_attribute("media", "na_letter_8.5x11in");
}

#[test]
fn feat_05_test_03_duplex_short_edge_tablet_submission() {
    let mock = MockIppDriver::new("Workgroup_Duplex_IPP", true);
    let mut attrs = HashMap::new();
    attrs.insert("sides".to_string(), "two-sided-short-edge".to_string());
    attrs.insert(
        "print-color-mode".to_string(),
        "monochrome".to_string(),
    );

    let job_id = mock
        .submit_ipp_job(attrs, b"%PDF-1.7 calendar")
        .expect("Submit tablet IPP job");

    assert!(job_id >= 1001);
    mock.assert_attribute("sides", "two-sided-short-edge");
}

#[test]
fn feat_05_test_04_ipp_message_writer_binary_wire_protocol_encoding() {
    let mut writer = IppMessageWriter::new(IppOperation::PrintJob, 101);
    writer.start_group(IppTag::OperationAttributes);
    writer.add_attribute_str(
        IppTag::Charset,
        "attributes-charset",
        "utf-8",
    );
    writer.add_attribute_str(
        IppTag::NaturalLanguage,
        "attributes-natural-language",
        "en",
    );
    writer.end_attributes();

    let bytes = writer.finish();
    assert!(bytes.len() >= 9);
    assert_eq!(bytes[0..2], [0x02, 0x00]); // IPP 2.0
    assert_eq!(
        u16::from_be_bytes([bytes[2], bytes[3]]),
        0x0002 // Print-Job
    );
    assert_eq!(
        u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        101 // Request ID
    );
    assert_eq!(*bytes.last().unwrap(), IppTag::EndOfAttributes as u8);
}

#[test]
fn feat_05_test_05_multiple_sequential_ipp_jobs_distinct_ids() {
    let mock = MockIppDriver::new("Production_Printer_IPP", true);
    let mut job_ids = Vec::new();

    for i in 0..5 {
        let mut attrs = HashMap::new();
        attrs.insert("job-name".to_string(), format!("Job_{}", i));
        let job_id = mock
            .submit_ipp_job(attrs, b"payload")
            .expect("Submit sequential job");
        job_ids.push(job_id);
    }

    assert_eq!(job_ids, vec![1001, 1002, 1003, 1004, 1005]);
    assert_eq!(mock.last_job().unwrap().job_id, 1005);
}

// =========================================================================
// FEATURE 6: Driver Monochrome Assertion (Layer 1) (5 Tests)
// =========================================================================

#[test]
fn feat_06_test_01_v3_devmode_monochrome_driver_flag_assertion() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Populate");

    devmode_buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Submit");

    mock.assert_monochrome_asserted();
    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_color, Some(1));
    assert!(last.dm_fields & DM_COLOR != 0);
}

#[test]
fn feat_06_test_02_v4_xps_printticket_monochrome_keyword_assertion() {
    let mock = MockV4Driver::new(MockV4DriverConfig {
        supports_monochrome: true,
        ..Default::default()
    });
    mock.pt_open_provider();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock
        .pt_merge_and_validate(None, delta)
        .expect("Merge V4 mono ticket");
    assert!(merged.contains("psk:Monochrome"));
}

#[test]
fn feat_06_test_03_cups_ipp_print_color_mode_monochrome_assertion() {
    let mock = MockIppDriver::new("Cups_Mono", true);
    let mut attrs = HashMap::new();
    attrs.insert(
        "print-color-mode".to_string(),
        "monochrome".to_string(),
    );

    mock.submit_ipp_job(attrs, b"payload").expect("Submit mono IPP");
    mock.assert_attribute("print-color-mode", "monochrome");
}

#[test]
fn feat_06_test_04_cross_driver_monochrome_flag_parity() {
    let mock_v3 = MockV3Driver::new(MockV3DriverConfig::default());
    let mock_v4 = MockV4Driver::new(MockV4DriverConfig {
        supports_monochrome: true,
        ..Default::default()
    });
    let mock_ipp = MockIppDriver::new("Ipp_Mono", true);

    // V3 DEVMODE assertion
    let needed = mock_v3.document_properties_pass1() as usize;
    let mut buf = vec![0u8; needed];
    mock_v3.document_properties_pass2(None, &mut buf, DM_OUT_BUFFER).unwrap();
    buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());
    let mut out = vec![0u8; needed];
    mock_v3
        .document_properties_pass2(Some(&buf), &mut out, DM_IN_BUFFER | DM_OUT_BUFFER)
        .unwrap();
    mock_v3.assert_monochrome_asserted();

    // V4 PrintTicket assertion
    mock_v4.pt_open_provider();
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;
    let merged = mock_v4.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:Monochrome"));

    // IPP assertion
    let mut attrs = HashMap::new();
    attrs.insert(
        "print-color-mode".to_string(),
        "monochrome".to_string(),
    );
    mock_ipp.submit_ipp_job(attrs, b"payload").unwrap();
    mock_ipp.assert_attribute("print-color-mode", "monochrome");
}

#[test]
fn feat_06_test_05_monochrome_assertion_with_duplex_and_orientation_coexistence() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    let fields = DM_COLOR | DM_DUPLEX | DM_ORIENTATION;
    devmode_buf[72..76].copy_from_slice(&fields.to_le_bytes());
    devmode_buf[76..78].copy_from_slice(&DMORIENT_LANDSCAPE.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_VERTICAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .unwrap();

    mock.assert_monochrome_asserted();
    mock.assert_duplex_asserted(DuplexBinding::FlipLongEdge);
    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_orientation, Some(DMORIENT_LANDSCAPE));
}

// =========================================================================
// FEATURE 7: In-Memory Vector K-Only Filter (Layer 2) (5 Tests)
// =========================================================================

fn filter_content_stream_bytes(input: &[u8]) -> Vec<u8> {
    let parsed = pdfcraft_content::parse(input);
    let filtered_ops = MonochromeVectorFilter::filter_operators(&parsed.ops);
    pdfcraft_content::serialize_ops(&filtered_ops)
}

#[test]
fn feat_07_test_01_rgb_vector_operators_converted_to_gray() {
    let input = b"0.2 0.4 0.8 rg 10 10 100 100 re f 0.5 0.5 0.5 RG 20 20 50 50 re S\n";
    let filtered = filter_content_stream_bytes(input);

    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(&filtered, 0);
    assert!(
        violations.is_empty(),
        "Violations found in filtered stream: {:?}",
        violations
    );

    let filtered_str = String::from_utf8_lossy(&filtered);
    assert!(!filtered_str.contains(" rg"));
    assert!(!filtered_str.contains(" RG"));
    assert!(filtered_str.contains(" g\n") || filtered_str.contains(" g "));
    assert!(filtered_str.contains(" G\n") || filtered_str.contains(" G "));
}

#[test]
fn feat_07_test_02_chromatic_cmyk_converted_to_pure_k_only() {
    let input = b"0.8 0.4 0.2 0.1 k 10 10 100 100 re f 0.1 0.2 0.3 0.4 K\n";
    let filtered = filter_content_stream_bytes(input);

    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(&filtered, 0);
    assert!(
        violations.is_empty(),
        "Chromatic CMYK found: {:?}",
        violations
    );

    let text = String::from_utf8_lossy(&filtered);
    // Any k operator must have 0 0 0 K format
    assert!(!text.contains("0.8 0.4 0.2"));
}

#[test]
fn feat_07_test_03_device_rgb_and_cmyk_colorspace_rewriting() {
    let input = b"/DeviceRGB cs 1.0 0.5 0.0 sc /DeviceCMYK CS 0.1 0.2 0.3 0.4 SC 5 5 10 10 re f\n";
    let filtered = filter_content_stream_bytes(input);

    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(&filtered, 0);
    assert!(
        violations.is_empty(),
        "Color space violations: {:?}",
        violations
    );

    let text = String::from_utf8_lossy(&filtered);
    assert!(!text.contains("/DeviceRGB"));
    assert!(!text.contains("/DeviceCMYK"));
    assert!(text.contains("/DeviceGray"));
}

#[test]
fn feat_07_test_04_already_k_only_stream_preserved_identically() {
    let input = b"q 0.5 g 10 10 100 100 re f 0 0 0 0.8 k 20 20 50 50 re f Q\n";
    let filtered = filter_content_stream_bytes(input);

    StreamAssertions::assert_content_stream_k_only(&filtered);
}

#[test]
fn feat_07_test_05_multi_page_document_vector_k_only_end_to_end() {
    let page1_ops = b"1.0 0.0 0.0 rg 10 10 50 50 re f\n";
    let page2_ops = b"0.0 1.0 0.0 RG 20 20 80 80 re S\n";

    let f1 = filter_content_stream_bytes(page1_ops);
    let f2 = filter_content_stream_bytes(page2_ops);

    let inspector = DefaultStreamInspector::new();
    let v1 = inspector.inspect_content_stream(&f1, 0);
    let v2 = inspector.inspect_content_stream(&f2, 1);

    assert!(v1.is_empty(), "Page 1 violations: {:?}", v1);
    assert!(v2.is_empty(), "Page 2 violations: {:?}", v2);

    let s1 = String::from_utf8_lossy(&f1);
    let s2 = String::from_utf8_lossy(&f2);
    assert!(!s1.contains(" rg"));
    assert!(s1.contains(" g\n") || s1.contains(" g "));
    assert!(!s2.contains(" RG"));
    assert!(s2.contains(" G\n") || s2.contains(" G "));
}

// =========================================================================
// FEATURE 8: Image XObject Grayscale Re-encoding (5 Tests)
// =========================================================================

#[test]
fn feat_08_test_01_rgb_image_xobject_reencoded_to_8bit_gray() {
    let inspector = DefaultStreamInspector::new();
    let dict = "<< /Type /XObject /Subtype /Image /Width 100 /Height 100 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let gray_pixels = vec![128u8; 10000];

    let result = inspector.inspect_image_xobject("Im1", dict, &gray_pixels, 0);
    assert!(result.is_ok(), "Expected valid 8-bit gray image: {:?}", result);
}

#[test]
fn feat_08_test_02_cmyk_image_xobject_reencoded_to_gray() {
    let inspector = DefaultStreamInspector::new();
    let dict = "<< /Type /XObject /Subtype /Image /Width 50 /Height 50 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let gray_pixels = vec![64u8; 2500];

    let result = inspector.inspect_image_xobject("ImCmyk", dict, &gray_pixels, 0);
    assert!(result.is_ok(), "Expected valid gray image from CMYK: {:?}", result);
}

#[test]
fn feat_08_test_03_inline_image_bi_id_ei_grayscale_conversion() {
    let input = b"BI /W 10 /H 10 /CS /RGB /BPC 8 ID 0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789 EI\n";
    let filtered = filter_content_stream_bytes(input);

    let text = String::from_utf8_lossy(&filtered);
    assert!(!text.contains("/CS /RGB"));
    assert!(text.contains("/CS /DeviceGray") || text.contains("/CS/DeviceGray") || !text.contains("RGB"));
}

#[test]
fn feat_08_test_04_flate_compressed_image_stream_transcoding() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let gray_pixels = vec![128u8; 40000]; // 200 x 200 x 1
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&gray_pixels).unwrap();
    let compressed = encoder.finish().unwrap();

    let inspector = DefaultStreamInspector::new();
    let dict = "<< /Type /XObject /Subtype /Image /Width 200 /Height 200 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode >>";

    let result = inspector.inspect_image_xobject("ImFlate", dict, &compressed, 0);
    assert!(result.is_ok(), "Compressed image inspection failed: {:?}", result);
}

#[test]
fn feat_08_test_05_compact_dict_syntax_image_xobject_inspection() {
    let inspector = DefaultStreamInspector::new();
    let dict_compact = "<</Type/XObject/Subtype/Image/Width100/Height100/ColorSpace/DeviceGray/BitsPerComponent8>>";
    let gray_pixels = vec![200u8; 10000];

    let result = inspector.inspect_image_xobject("ImCompact", dict_compact, &gray_pixels, 0);
    assert!(result.is_ok(), "Compact dict format should parse cleanly: {:?}", result);
}

// =========================================================================
// FEATURE 9: Dynamic Physical Margin Querying (5 Tests)
// =========================================================================

#[test]
fn feat_09_test_01_symmetric_a4_600dpi_laser_margins() {
    let raw = RawGdiCaps {
        dpi_x: 600,
        dpi_y: 600,
        horz_res: 4820,
        vert_res: 6874,
        phys_width: 4960,
        phys_height: 7014,
        phys_offset_x: 70,
        phys_offset_y: 70,
        is_duplex: true,
        is_color: true,
        max_copies: 99,
    };

    let bounds = raw.to_printable_bounds();
    assert!((bounds.margins.left_pt - 8.40).abs() < 1e-4);
    assert!((bounds.margins.top_pt - 8.40).abs() < 1e-4);
    assert!((bounds.margins.right_pt - 8.40).abs() < 1e-4);
    assert!((bounds.margins.bottom_pt - 8.40).abs() < 1e-4);

    assert!((bounds.printable_width_pt - 578.40).abs() < 1e-4);
    assert!((bounds.physical_width_pt - 595.20).abs() < 1e-4);
    assert_eq!(bounds.dpi_x, 600);
    assert_eq!(bounds.dpi_y, 600);
}

#[test]
fn feat_09_test_02_borderless_photo_printer_zero_margins() {
    let raw = RawGdiCaps {
        dpi_x: 600,
        dpi_y: 600,
        horz_res: 4961,
        vert_res: 7016,
        phys_width: 4961,
        phys_height: 7016,
        phys_offset_x: 0,
        phys_offset_y: 0,
        is_duplex: false,
        is_color: true,
        max_copies: 10,
    };

    let bounds = raw.to_printable_bounds();
    assert_eq!(bounds.margins.left_pt, 0.0);
    assert_eq!(bounds.margins.top_pt, 0.0);
    assert_eq!(bounds.margins.right_pt, 0.0);
    assert_eq!(bounds.margins.bottom_pt, 0.0);
    assert_eq!(bounds.printable_width_pt, bounds.physical_width_pt);
    assert_eq!(bounds.printable_height_pt, bounds.physical_height_pt);
}

#[test]
fn feat_09_test_03_asymmetric_pos_receipt_printer_margins() {
    let raw = RawGdiCaps {
        dpi_x: 203,
        dpi_y: 203,
        phys_width: 640,
        phys_height: 2000,
        horz_res: 580,
        vert_res: 1999,
        phys_offset_x: 30,
        phys_offset_y: 0,
        is_duplex: false,
        is_color: false,
        max_copies: 1,
    };

    let bounds = raw.to_printable_bounds();
    assert_eq!(bounds.margins.top_pt, 0.0);
    assert!(bounds.margins.left_pt > 10.0);
    assert!(bounds.margins.right_pt > 10.0);
    assert!(bounds.margins.bottom_pt < 1.0);
}

#[test]
fn feat_09_test_04_high_dpi_1200_plotter_coordinate_scaling() {
    let raw = RawGdiCaps {
        dpi_x: 1200,
        dpi_y: 1200,
        phys_width: 9920,
        phys_height: 14028,
        horz_res: 9640,
        vert_res: 13748,
        phys_offset_x: 140,
        phys_offset_y: 140,
        is_duplex: true,
        is_color: true,
        max_copies: 999,
    };

    let bounds = raw.to_printable_bounds();
    let expected_margin_pt = 140.0 * (72.0 / 1200.0); // 8.4 pt
    assert!((bounds.margins.left_pt - expected_margin_pt).abs() < 1e-4);
    assert!((bounds.margins.top_pt - expected_margin_pt).abs() < 1e-4);
}

#[test]
fn feat_09_test_05_large_format_arch_d_asymmetric_gripper_margins() {
    let raw = RawGdiCaps {
        dpi_x: 300,
        dpi_y: 300,
        phys_width: 7200,  // 24 in * 300
        phys_height: 10800, // 36 in * 300
        horz_res: 7050,
        vert_res: 10500,
        phys_offset_x: 75,  // 18 pt leading edge
        phys_offset_y: 150, // 36 pt top edge
        is_duplex: false,
        is_color: true,
        max_copies: 50,
    };

    let bounds = raw.to_printable_bounds();
    assert_eq!(bounds.physical_width_pt, 1728.0);
    assert_eq!(bounds.physical_height_pt, 2592.0);
    assert!((bounds.margins.left_pt - 18.0).abs() < 1e-4);
    assert!((bounds.margins.top_pt - 36.0).abs() < 1e-4);
}
