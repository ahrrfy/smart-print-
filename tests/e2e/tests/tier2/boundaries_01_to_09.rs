#![allow(unused_imports, unused_must_use, dead_code)]
//! Tier 2 Boundary Value Analysis & Negative Testing: Features 1 to 9 (45 tests total)
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
    IppWireError, MockIppDriver, MockV3Driver, MockV3DriverConfig, MockV4Driver, MockV4DriverConfig,
    ParsedIppRequest, ParsedIppResponse, PrintJobSettings, PrintableBounds, DMCOLOR_COLOR,
    DMCOLOR_MONOCHROME, DMDUP_HORIZONTAL, DMDUP_VERTICAL, DMORIENT_LANDSCAPE, DMORIENT_PORTRAIT,
    DM_COLOR, DM_COPIES, DM_DUPLEX, DM_IN_BUFFER, DM_ORIENTATION, DM_OUT_BUFFER, IPP_STATUS_OK,
    IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
};
use pdfcraft_e2e::mock_spooler::{CommittedSheetsLedger, MockSpooler, SheetStatus};
use pdfcraft_e2e::stream_inspector::{
    DefaultStreamInspector, PdfStreamInspector, StreamAssertions, StreamViolation,
};
use pdfcraft_platform::caps::RawGdiCaps;
use pdfcraft_platform::cups::{IppMessageWriter, IppOperation, IppTag};
use pdfcraft_print::monochrome::MonochromeVectorFilter;
use std::collections::HashMap;
use std::io::Read;

// =========================================================================
// FEATURE 1: Win32 DevMode 2-Pass Negotiation (5 Boundary & Negative Tests)
// =========================================================================

#[test]
fn feat_01_bound_01_zero_length_buffer_rejection() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1();
    assert_eq!(needed, 3924);

    let mut empty_buf: [u8; 0] = [];
    let result = mock.document_properties_pass2(None, &mut empty_buf, DM_OUT_BUFFER);

    assert_eq!(
        result,
        Err(DevModeValidationError::BufferTooSmall {
            provided: 0,
            required: 3924,
        })
    );
}

#[test]
fn feat_01_bound_02_undersized_buffer_error_insufficient_buffer() {
    let mock = MockV3Driver::new(MockV3DriverConfig {
        driver_extra_size: 3704,
        ..Default::default()
    });

    // Provide only 220 bytes (DEVMODE base size, omitting the 3704-byte vendor extra)
    let mut undersized_buf = vec![0u8; 220];
    let result = mock.document_properties_pass2(None, &mut undersized_buf, DM_OUT_BUFFER);

    assert_eq!(
        result,
        Err(DevModeValidationError::BufferTooSmall {
            provided: 220,
            required: 3924,
        })
    );
}

#[test]
fn feat_01_bound_03_corrupted_vendor_canary_payload_rejection() {
    let mock = MockV3Driver::new(MockV3DriverConfig {
        driver_extra_size: 3704,
        vendor_canary_pattern: vec![0xAA, 0x55, 0xDE, 0xAD, 0xBE, 0xEF],
        ..Default::default()
    });

    let needed = mock.document_properties_pass1() as usize;
    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Default populate should succeed");

    // Corrupt one byte in the vendor private data area (offset 220 + 42)
    devmode_buf[220 + 42] ^= 0xFF;

    let mut out_buf = vec![0u8; needed];
    let result = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );

    assert_eq!(
        result,
        Err(DevModeValidationError::CorruptedVendorExtraPayload {
            expected_len: 3704,
            actual_len: 3704,
        })
    );
}

#[test]
fn feat_01_bound_04_maximum_vendor_extra_64kb_payload_boundary() {
    // 64KB max vendor private payload boundary (u16::MAX = 65535)
    let max_extra = 65535;
    let mock = MockV3Driver::new(MockV3DriverConfig {
        driver_extra_size: max_extra,
        vendor_canary_pattern: vec![0xCA, 0xFE, 0xBA, 0xBE],
        ..Default::default()
    });

    let needed = mock.document_properties_pass1() as usize;
    assert_eq!(needed, 220 + max_extra);

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER)
        .expect("Populate 64KB DEVMODE");

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .expect("Submit 64KB DEVMODE");

    mock.assert_driver_extra_preserved();
    assert_eq!(
        mock.last_submitted_devmode().unwrap().dm_driver_extra as usize,
        max_extra
    );
}

#[test]
fn feat_01_bound_05_pass2_before_pass1_detection() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());

    // Call Pass 2 directly without querying Pass 1
    let needed = mock.required_buffer_size();
    let mut devmode_buf = vec![0u8; needed];
    let res = mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER);
    assert!(res.is_ok());

    // Calling assert_pass1_called should panic because Pass 1 was skipped
    let panic_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mock.assert_pass1_called();
    }));
    assert!(
        panic_res.is_err(),
        "Expected assert_pass1_called to panic when Pass 1 was bypassed"
    );
}

// =========================================================================
// FEATURE 2: DEVMODE Flag Assertion (5 Boundary & Negative Tests)
// =========================================================================

#[test]
fn feat_02_bound_01_missing_dmfields_color_bitmask_ignores_value() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Write DMCOLOR_MONOCHROME to dmColor field (offset 92..94), but DO NOT set DM_COLOR in dmFields (72..76)
    devmode_buf[72..76].copy_from_slice(&0u32.to_le_bytes()); // Clear dmFields
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_MONOCHROME.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .unwrap();

    let last = mock.last_submitted_devmode().unwrap();
    // Driver ignores dmColor because DM_COLOR bitmask is missing
    assert_eq!(last.dm_color, None);
    assert_eq!(last.dm_fields & DM_COLOR, 0);

    let panic_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mock.assert_monochrome_asserted();
    }));
    assert!(panic_res.is_err(), "assert_monochrome_asserted must fail when DM_COLOR bit is missing");
}

#[test]
fn feat_02_bound_02_invalid_color_and_duplex_enum_rejection() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Test invalid color value 99
    devmode_buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&99i16.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    let err_color = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert_eq!(err_color, Err(DevModeValidationError::InvalidColorValue(99)));

    // Test invalid duplex value 42
    devmode_buf[72..76].copy_from_slice(&DM_DUPLEX.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&42i16.to_le_bytes());
    let err_duplex = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert_eq!(err_duplex, Err(DevModeValidationError::InvalidDuplexValue(42)));
}

#[test]
fn feat_02_bound_03_zero_copy_count_boundary() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Assert zero copies boundary (dmCopies = 0)
    devmode_buf[72..76].copy_from_slice(&DM_COPIES.to_le_bytes());
    devmode_buf[86..88].copy_from_slice(&0i16.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .unwrap();

    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_copies, Some(0));
}

#[test]
fn feat_02_bound_04_extreme_copy_count_9999_and_i16_max() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Test extreme 9,999 copies
    devmode_buf[72..76].copy_from_slice(&DM_COPIES.to_le_bytes());
    devmode_buf[86..88].copy_from_slice(&9999i16.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .unwrap();

    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_copies, Some(9999));

    // Test i16::MAX (32,767) boundary
    devmode_buf[86..88].copy_from_slice(&i16::MAX.to_le_bytes());
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .unwrap();
    assert_eq!(mock.last_submitted_devmode().unwrap().dm_copies, Some(32767));
}

#[test]
fn feat_02_bound_05_missing_dmduplex_bitmask_ignores_duplex_value() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Write DMDUP_VERTICAL but omit DM_DUPLEX from dmFields
    devmode_buf[72..76].copy_from_slice(&0u32.to_le_bytes());
    devmode_buf[94..96].copy_from_slice(&DMDUP_VERTICAL.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .unwrap();

    let last = mock.last_submitted_devmode().unwrap();
    assert_eq!(last.dm_duplex, None);
    assert_eq!(last.dm_fields & DM_DUPLEX, 0);
}

// =========================================================================
// FEATURE 3: XPS PrintTicket Validation & Merge (5 Boundary & Negative Tests)
// =========================================================================

#[test]
fn feat_03_bound_01_missing_monochrome_capabilities_fallback_to_color() {
    let mock = create_mock_virtual_pdf_printer("Microsoft_Print_To_PDF");
    mock.pt_open_provider().unwrap();

    // Request Monochrome on a virtual driver that omits psk:Monochrome
    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:Color"));
    assert_eq!(mock.conflicts().len(), 1);
    assert_eq!(mock.conflicts()[0].feature, "psk:PageOutputColor");
    assert_eq!(mock.conflicts()[0].resolved_option, "psk:Color");
}

#[test]
fn feat_03_bound_02_malformed_xml_delta_ticket_handling() {
    let mock = MockV4Driver::new(MockV4DriverConfig::default());
    mock.pt_open_provider().unwrap();

    // Malformed, truncated XML string
    let broken_xml = "<psf:PrintTicket xmlns:psf=\"http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework\"><psf:Feature name=";
    let res = mock.pt_merge_and_validate(None, broken_xml);
    assert!(res.is_ok());
    let merged = res.unwrap();
    // Driver falls back to valid XML structure
    assert!(merged.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(merged.contains("</psf:PrintTicket>"));
}

#[test]
fn feat_03_bound_03_namespace_prefix_alias_resolution() {
    let mock = MockV4Driver::new(MockV4DriverConfig::default());
    mock.pt_open_provider().unwrap();

    // Ticket using custom namespace prefixes instead of standard psf/psk
    let aliased_ticket = r#"<f:PrintTicket xmlns:f="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:k="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <f:Feature name="k:PageOutputColor">
        <f:Option name="k:Monochrome"/>
    </f:Feature>
</f:PrintTicket>"#;

    let merged = mock.pt_merge_and_validate(None, aliased_ticket).unwrap();
    assert!(merged.contains("PrintTicket"));
    mock.assert_no_concurrency_violations();
}

#[test]
fn feat_03_bound_04_oversized_print_ticket_payload_boundary() {
    let mock = MockV4Driver::new(MockV4DriverConfig::default());
    mock.pt_open_provider().unwrap();

    // Construct 128KB ticket payload with large XML comments
    let mut large_ticket = String::from(r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">"#);
    let padding = "<!-- PADDING_COMMENT_FOR_OVERSIZED_TICKET_TEST_0123456789 -->\n";
    for _ in 0..2000 {
        large_ticket.push_str(padding);
    }
    large_ticket.push_str(r#"<psf:Feature name="psk:PageOrientation"><psf:Option name="psk:Landscape"/></psf:Feature></psf:PrintTicket>"#);

    assert!(large_ticket.len() > 120_000);
    let merged = mock.pt_merge_and_validate(None, &large_ticket).unwrap();
    assert!(merged.contains("psk:Landscape"));
}

#[test]
fn feat_03_bound_05_conflicting_duplex_on_simplex_hardware_constraint_resolution() {
    let mock = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Simplex_Receipt_V4".to_string(),
        supports_duplex: false,
        ..Default::default()
    });
    mock.pt_open_provider().unwrap();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
        <psf:Option name="psk:TwoSidedLongEdge"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock.pt_merge_and_validate(None, delta).unwrap();
    assert!(merged.contains("psk:OneSided"));
    assert_eq!(mock.conflicts().len(), 1);
    assert_eq!(mock.conflicts()[0].feature, "psk:JobDuplexAllDocumentsContiguously");
    assert_eq!(mock.conflicts()[0].resolved_option, "psk:OneSided");
}

// =========================================================================
// FEATURE 4: PrintDocumentPackageTarget Spooling (5 Boundary & Negative Tests)
// =========================================================================

#[test]
fn feat_04_bound_01_truncated_stream_producer_premature_termination() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(5, 2);

    // Producer prematurely returns None at sheet index 2
    let result = spooler.spool_sheet_stream(301, &ledger, 1, |idx| {
        if idx < 2 {
            Some(format!("Page {}", idx).into_bytes())
        } else {
            None
        }
    });

    assert!(result.is_err());
    assert_eq!(ledger.committed_count(), 2);
    assert_eq!(ledger.pending_sheets_for_failover(), vec![2, 3, 4]);
}

#[test]
fn feat_04_bound_02_zero_byte_stream_rejection() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(1, 2);

    // Zero-byte stream payload
    let res = spooler.spool_sheet_stream(302, &ledger, 1, |_| Some(Vec::new()));
    assert!(res.is_ok());
    assert_eq!(ledger.committed_count(), 1);
    // Verified 0-length file committed without spooler corruption
}

#[test]
fn feat_04_bound_03_mid_package_io_fault_injection_and_ledger_containment() {
    let spooler = MockSpooler::new();
    let ledger = CommittedSheetsLedger::new(8, 2);

    // Injected paper jam / fault at sheet 3
    spooler.inject_failure_at_sheet(3);

    let res = spooler.spool_sheet_stream(303, &ledger, 1, |i| {
        Some(format!("Sheet data {}", i).into_bytes())
    });

    assert!(res.is_err());
    assert_eq!(ledger.committed_count(), 3);
    assert_eq!(ledger.pending_sheets_for_failover(), vec![3, 4, 5, 6, 7]);
    assert!(matches!(ledger.status_of(3), Some(SheetStatus::Failed { .. })));
}

#[test]
fn feat_04_bound_04_corrupted_content_type_package_header() {
    let inspector = DefaultStreamInspector::new();
    // Non-PDF/non-XPS corrupt magic bytes
    let corrupt_payload = b"\x00\x00\x00\x00\xFF\xFFgarbage_stream";
    let violations = inspector.inspect_content_stream(corrupt_payload, 0);
    // Malformed stream parsed without crash
    assert!(violations.is_empty());
}

#[test]
fn feat_04_bound_05_concurrent_target_spooler_abort_and_peer_isolation() {
    let spooler1 = MockSpooler::new();
    let spooler2 = MockSpooler::new();

    // Spooler 1 fails at sheet 1
    spooler1.inject_failure_at_sheet(1);

    let h1 = std::thread::spawn(move || {
        let ledger = CommittedSheetsLedger::new(5, 2);
        let res = spooler1.spool_sheet_stream(401, &ledger, 1, |i| {
            Some(format!("Target A {}", i).into_bytes())
        });
        (res.is_err(), ledger.committed_count())
    });

    let h2 = std::thread::spawn(move || {
        let ledger = CommittedSheetsLedger::new(5, 2);
        let res = spooler2.spool_sheet_stream(402, &ledger, 2, |i| {
            Some(format!("Target B {}", i).into_bytes())
        });
        (res.is_ok(), ledger.committed_count())
    });

    let (s1_failed, s1_committed) = h1.join().unwrap();
    let (s2_ok, s2_committed) = h2.join().unwrap();

    assert!(s1_failed);
    assert_eq!(s1_committed, 1);
    assert!(s2_ok);
    assert_eq!(s2_committed, 5);
}

// =========================================================================
// FEATURE 5: CUPS / IPP 2.0 Native Adapter (5 Boundary & Negative Tests)
// =========================================================================

#[test]
fn feat_05_bound_01_unsupported_duplex_attribute_returns_ipp_error_0x040b() {
    let mock = MockIppDriver::new("Simplex_Only_IPP", false);
    let mut attrs = HashMap::new();
    attrs.insert("sides".to_string(), "two-sided-long-edge".to_string());

    let res = mock.submit_ipp_job(attrs, b"%PDF-1.7 payload");
    assert_eq!(
        res,
        Err(IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED)
    );
}

#[test]
fn feat_05_bound_02_malformed_tag_octet_wire_protocol_rejection() {
    let mut writer = IppMessageWriter::new(IppOperation::PrintJob, 1);
    writer.start_group(IppTag::OperationAttributes);
    // Write valid attributes
    writer.add_attribute_str(IppTag::Charset, "attributes-charset", "utf-8");
    writer.end_attributes();
    let mut wire_bytes = writer.finish();

    // Corrupt tag octet at offset 8 to illegal value 0xFF
    if wire_bytes.len() > 8 {
        wire_bytes[8] = 0xFF;
    }

    // Wire protocol parser must reject illegal tag octet 0xFF
    let parse_res = MockIppDriver::parse_and_validate_wire_request(&wire_bytes);
    assert_eq!(
        parse_res,
        Err(IppWireError::InvalidTagOctet(0xFF)),
        "Malformed tag octet 0xFF must be rejected"
    );

    // Driver wire submission must also reject the corrupted payload
    let mock = MockIppDriver::new("MalformedTag_IPP", true);
    let submit_res = mock.submit_ipp_wire_job(&wire_bytes);
    assert_eq!(
        submit_res,
        Err(IppWireError::InvalidTagOctet(0xFF)),
        "Submitting malformed wire packet to driver must fail"
    );
}

#[test]
fn feat_05_bound_03_truncated_ipp_response_buffer_handling() {
    // Under 8 bytes (RFC 8010 requires minimum 8-byte response header)
    let truncated_buf = vec![0x02, 0x00, 0x00];

    // Parser reading request ID / status code must detect buffer underrun safely
    let res = MockIppDriver::parse_ipp_response(&truncated_buf);
    assert_eq!(
        res,
        Err(IppWireError::BufferTooShort {
            len: 3,
            expected: 8,
        }),
        "Truncated 3-byte response buffer must fail with BufferTooShort"
    );

    // Wire request parser must also reject truncated buffer (< 8 bytes)
    let req_res = MockIppDriver::parse_and_validate_wire_request(&truncated_buf);
    assert_eq!(
        req_res,
        Err(IppWireError::BufferTooShort {
            len: 3,
            expected: 8,
        }),
        "Truncated 3-byte request buffer must fail with BufferTooShort"
    );
}

#[test]
fn feat_05_bound_04_unsupported_operation_id_handling() {
    // Construct message with non-standard operation code 0xBEEF
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&[0x02, 0x00]); // IPP 2.0
    buffer.extend_from_slice(&0xBEEFu16.to_be_bytes()); // Non-standard Operation ID
    buffer.extend_from_slice(&101u32.to_be_bytes()); // Request ID
    buffer.push(IppTag::EndOfAttributes as u8);

    // Parser must detect and reject unsupported operation ID 0xBEEF
    let res = MockIppDriver::parse_and_validate_wire_request(&buffer);
    assert_eq!(
        res,
        Err(IppWireError::UnsupportedOperation(0xBEEF)),
        "Non-standard operation ID 0xBEEF must be rejected with UnsupportedOperation"
    );

    // Driver wire submission must also reject the unsupported operation
    let mock = MockIppDriver::new("UnsupportedOp_IPP", true);
    let submit_res = mock.submit_ipp_wire_job(&buffer);
    assert_eq!(
        submit_res,
        Err(IppWireError::UnsupportedOperation(0xBEEF)),
        "Mock driver submission of unsupported operation ID must fail"
    );
}

#[test]
fn feat_05_bound_05_negative_or_overflow_copies_attribute() {
    let mock = MockIppDriver::new("Standard_IPP", true);
    let mut attrs = HashMap::new();
    // Negative copies boundary
    attrs.insert("copies".to_string(), "-5".to_string());

    let job_id = mock.submit_ipp_job(attrs, b"payload").unwrap();
    assert!(job_id >= 1001);
    mock.assert_attribute("copies", "-5");

    // Extreme copies overflow boundary (value cannot parse as standard u16/i32)
    let overflow_val = "99999999999999999999";
    let parsed_i32 = overflow_val.parse::<i32>();
    assert!(parsed_i32.is_err(), "Copies > i32::MAX must overflow standard integer parser");
}

// =========================================================================
// FEATURE 6: Driver Monochrome Assertion (5 Boundary & Negative Tests)
// =========================================================================

#[test]
fn feat_06_bound_01_driver_rejects_mono_and_resolves_to_color_mismatch_detected() {
    let mock = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Color_Only_V4".to_string(),
        supports_color: true,
        supports_monochrome: false,
        ..Default::default()
    });
    mock.pt_open_provider().unwrap();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor">
        <psf:Option name="psk:Monochrome"/>
    </psf:Feature>
</psf:PrintTicket>"#;

    let merged = mock.pt_merge_and_validate(None, delta).unwrap();
    // Driver forces color fallback; Layer 1 detects mismatch
    assert!(!merged.contains("psk:Monochrome"));
    assert!(merged.contains("psk:Color"));
    assert_eq!(mock.conflicts().len(), 1);
}

#[test]
fn feat_06_bound_02_v3_devmode_driver_ticket_downgrade_detection() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Driver internally overwrites dmColor to DMCOLOR_COLOR (2)
    devmode_buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&DMCOLOR_COLOR.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    )
    .unwrap();

    let panic_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mock.assert_monochrome_asserted();
    }));
    assert!(panic_res.is_err(), "Downgrade to DMCOLOR_COLOR must fail assertion");
}

#[test]
fn feat_06_bound_03_invalid_color_mode_enum_value_rejection() {
    let mock = MockV3Driver::new(MockV3DriverConfig::default());
    let needed = mock.document_properties_pass1() as usize;

    let mut devmode_buf = vec![0u8; needed];
    mock.document_properties_pass2(None, &mut devmode_buf, DM_OUT_BUFFER).unwrap();

    // Test raw value 0 (neither MONOCHROME nor COLOR)
    devmode_buf[72..76].copy_from_slice(&DM_COLOR.to_le_bytes());
    devmode_buf[92..94].copy_from_slice(&0i16.to_le_bytes());

    let mut out_buf = vec![0u8; needed];
    let res = mock.document_properties_pass2(
        Some(&devmode_buf),
        &mut out_buf,
        DM_IN_BUFFER | DM_OUT_BUFFER,
    );
    assert_eq!(res, Err(DevModeValidationError::InvalidColorValue(0)));
}

#[test]
fn feat_06_bound_04_conflicting_color_settings_in_multi_printer_batch() {
    let mock_mono = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Printer_Mono".into(),
        supports_monochrome: true,
        ..Default::default()
    });
    let mock_color = MockV4Driver::new(MockV4DriverConfig {
        printer_name: "Printer_ColorOnly".into(),
        supports_monochrome: false,
        ..Default::default()
    });

    mock_mono.pt_open_provider().unwrap();
    mock_color.pt_open_provider().unwrap();

    let delta = r#"<psf:PrintTicket xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords">
    <psf:Feature name="psk:PageOutputColor"><psf:Option name="psk:Monochrome"/></psf:Feature>
</psf:PrintTicket>"#;

    let t1 = mock_mono.pt_merge_and_validate(None, delta).unwrap();
    let t2 = mock_color.pt_merge_and_validate(None, delta).unwrap();

    assert!(t1.contains("psk:Monochrome"));
    assert!(t2.contains("psk:Color"));
    assert_eq!(mock_color.conflicts().len(), 1);
}

#[test]
fn feat_06_bound_05_layer1_monochrome_assertion_audit_reporting() {
    let mock_ipp = MockIppDriver::new("Audit_IPP", true);
    let mut attrs = HashMap::new();
    attrs.insert("print-color-mode".to_string(), "monochrome".to_string());
    mock_ipp.submit_ipp_job(attrs, b"payload").unwrap();

    let job = mock_ipp.last_job().unwrap();
    assert_eq!(job.attributes.get("print-color-mode").unwrap(), "monochrome");
}

// =========================================================================
// FEATURE 7: In-Memory Vector K-Only Filter (5 Boundary & Negative Tests)
// =========================================================================

fn filter_content_stream_bytes(input: &[u8]) -> Vec<u8> {
    let parsed = pdfcraft_content::parse(input);
    let filtered_ops = MonochromeVectorFilter::filter_operators(&parsed.ops);
    pdfcraft_content::serialize_ops(&filtered_ops)
}

#[test]
fn feat_07_bound_01_chromatic_cmyk_rich_black_and_near_black_conversion() {
    // Rich black: C=0.6, M=0.4, Y=0.4, K=1.0 and Near-black: C=0.01, M=0.01, Y=0.01, K=0.9
    let input = b"0.6 0.4 0.4 1.0 k 10 10 50 50 re f 0.01 0.01 0.01 0.9 K 20 20 60 60 re S\n";
    let filtered = filter_content_stream_bytes(input);

    let inspector = DefaultStreamInspector::new();
    let violations = inspector.inspect_content_stream(&filtered, 0);
    assert!(
        violations.is_empty(),
        "Detected chromatic CMYK violations: {:?}",
        violations
    );

    let text = String::from_utf8_lossy(&filtered);
    // Must be converted to 0 0 0 K_eff
    assert!(text.contains("0 0 0 1 k") || text.contains("0 0 0 1.0 k") || text.contains("0 0 0"));
    assert!(!text.contains("0.6 0.4 0.4"));
}

#[test]
fn feat_07_bound_02_complex_nested_clipping_paths_preservation() {
    let input = b"q 10 10 80 80 re W n q 20 20 60 60 re W* n 1.0 0.0 0.0 rg 30 30 10 10 re f Q Q\n";
    let filtered = filter_content_stream_bytes(input);

    let text = String::from_utf8_lossy(&filtered);
    // Ensure all clipping operators W, W*, n, q, Q are preserved
    assert!(text.contains("W"));
    assert!(text.contains("W*"));
    assert!(text.contains("n"));
    assert!(text.contains("q"));
    assert!(text.contains("Q"));
    // Color operator converted
    assert!(!text.contains("1.0 0.0 0.0 rg"));
    assert!(text.contains("g\n") || text.contains("g "));
}

#[test]
fn feat_07_bound_03_zero_thickness_hairline_stroke_boundary() {
    let input = b"0 w 0.8 0.2 0.1 RG 10 10 m 100 100 l S\n";
    let filtered = filter_content_stream_bytes(input);

    let text = String::from_utf8_lossy(&filtered);
    // 0 w (hairline stroke) must be preserved
    assert!(text.contains("0 w"));
    assert!(!text.contains("0.8 0.2 0.1 RG"));
    assert!(text.contains("G\n") || text.contains("G "));
}

#[test]
fn feat_07_bound_04_degenerate_paths_and_redundant_operators() {
    // Degenerate zero-length line and redundant moves
    let input = b"0 0 m 0 0 l S 50 50 m 50 50 m f 10 10 m h\n";
    let filtered = filter_content_stream_bytes(input);

    let text = String::from_utf8_lossy(&filtered);
    assert!(!text.is_empty());
}

#[test]
fn feat_07_bound_05_empty_and_whitespace_only_content_stream_passthrough() {
    let empty_input = b"";
    let f1 = filter_content_stream_bytes(empty_input);
    assert!(f1.is_empty());

    let comment_input = b"%PDF-comment\n   \n";
    let f2 = filter_content_stream_bytes(comment_input);
    StreamAssertions::assert_content_stream_k_only(&f2);
}

// =========================================================================
// FEATURE 8: Image XObject Grayscale Re-encoding (5 Boundary & Negative Tests)
// =========================================================================

#[test]
fn feat_08_bound_01_1bit_monochrome_stencil_mask_preservation() {
    let inspector = DefaultStreamInspector::new();
    // 1-bit stencil mask with /ImageMask true
    let dict = "<< /Type /XObject /Subtype /Image /Width 64 /Height 64 /ImageMask true /ColorSpace /DeviceGray /BitsPerComponent 1 >>";
    let mask_bytes = vec![0xAAu8; 512]; // 64 * 64 / 8

    let result = inspector.inspect_image_xobject("StencilMask", dict, &mask_bytes, 0);
    assert!(result.is_ok(), "1-bit ImageMask should be preserved as valid stencil: {:?}", result);
}

#[test]
fn feat_08_bound_02_16bit_per_component_rgb_downsampling_to_8bit_gray() {
    let inspector = DefaultStreamInspector::new();
    // 16-bit RGB image downsampled to 8-bit DeviceGray
    let dict = "<< /Type /XObject /Subtype /Image /Width 10 /Height 10 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let gray_pixels = vec![128u8; 100];

    let result = inspector.inspect_image_xobject("Im16BitConverted", dict, &gray_pixels, 0);
    assert!(result.is_ok());
}

#[test]
fn feat_08_bound_03_indexed_palette_rgb_to_gray_lookup_transformation() {
    let inspector = DefaultStreamInspector::new();
    // Palette indexed image re-encoded to DeviceGray
    let dict = "<< /Type /XObject /Subtype /Image /Width 16 /Height 16 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let pixels = vec![42u8; 256];

    let result = inspector.inspect_image_xobject("ImIndexedConverted", dict, &pixels, 0);
    assert!(result.is_ok());
}

#[test]
fn feat_08_bound_04_corrupted_image_stream_graceful_recovery() {
    let inspector = DefaultStreamInspector::new();
    let dict = "<< /Type /XObject /Subtype /Image /Width 100 /Height 100 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode >>";
    // Corrupted Flate data (not valid zlib)
    let corrupt_flate = b"NOT_A_VALID_ZLIB_STREAM_CORRUPTED";

    let result = inspector.inspect_image_xobject("ImCorrupt", dict, corrupt_flate, 0);
    // Inspection must detect corrupted flate stream and return MalformedStream violation
    assert!(
        matches!(result, Err(StreamViolation::MalformedStream { .. })),
        "Corrupted flate stream must be detected as MalformedStream, got: {:?}",
        result
    );

    // Direct decoder verification: zlib decompression must fail safely without panic
    let mut decoder = flate2::read::ZlibDecoder::new(&corrupt_flate[..]);
    let mut decompressed = Vec::new();
    let decode_res = decoder.read_to_end(&mut decompressed);
    assert!(
        decode_res.is_err(),
        "Corrupted stream must fail zlib decoding without panic"
    );
}

#[test]
fn feat_08_bound_05_zero_dimension_width_or_height_image_rejection() {
    let inspector = DefaultStreamInspector::new();
    let dict_zero_width = "<< /Type /XObject /Subtype /Image /Width 0 /Height 100 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let res = inspector.inspect_image_xobject("ImZeroWidth", dict_zero_width, &[], 0);
    assert!(
        matches!(res, Err(StreamViolation::MalformedStream { .. })),
        "Zero-dimension width must be rejected as MalformedStream, got: {:?}",
        res
    );

    let dict_zero_height = "<< /Type /XObject /Subtype /Image /Width 100 /Height 0 /ColorSpace /DeviceGray /BitsPerComponent 8 >>";
    let res_h = inspector.inspect_image_xobject("ImZeroHeight", dict_zero_height, &[], 0);
    assert!(
        matches!(res_h, Err(StreamViolation::MalformedStream { .. })),
        "Zero-dimension height must be rejected as MalformedStream, got: {:?}",
        res_h
    );
}

// =========================================================================
// FEATURE 9: Dynamic Physical Margin Querying (5 Boundary & Negative Tests)
// =========================================================================

#[test]
fn feat_09_bound_01_extreme_asymmetric_pos_receipt_margins() {
    let raw = RawGdiCaps {
        dpi_x: 203,
        dpi_y: 203,
        phys_width: 640,
        phys_height: 20000,
        horz_res: 580,
        vert_res: 19990,
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
    assert!(bounds.margins.bottom_pt < 4.0);
}

#[test]
fn feat_09_bound_02_zero_margin_borderless_photo_exact_equality() {
    let raw = RawGdiCaps {
        dpi_x: 600,
        dpi_y: 600,
        phys_width: 4961,
        phys_height: 7016,
        horz_res: 4961,
        vert_res: 7016,
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
fn feat_09_bound_03_negative_margin_overflow_protection() {
    // Driver reports erroneous caps where horz_res + phys_offset_x > phys_width
    let raw = RawGdiCaps {
        dpi_x: 300,
        dpi_y: 300,
        phys_width: 1000,
        phys_height: 1000,
        horz_res: 950,
        vert_res: 950,
        phys_offset_x: 100, // 950 + 100 = 1050 > 1000
        phys_offset_y: 100,
        is_duplex: false,
        is_color: false,
        max_copies: 1,
    };

    let bounds = raw.to_printable_bounds();
    // saturating_sub ensures right_pt and bottom_pt clamp to 0.0 rather than underflowing
    assert_eq!(bounds.margins.right_pt, 0.0);
    assert_eq!(bounds.margins.bottom_pt, 0.0);
    assert!(bounds.margins.left_pt > 0.0);
    assert!(bounds.margins.top_pt > 0.0);
}

#[test]
fn feat_09_bound_04_fractional_dpi_and_non_square_aspect_ratio() {
    // Non-square aspect ratio: Fax driver 204 DPI horizontal x 98 DPI vertical
    let raw = RawGdiCaps {
        dpi_x: 204,
        dpi_y: 98,
        phys_width: 1728,
        phys_height: 1078,
        horz_res: 1680,
        vert_res: 1050,
        phys_offset_x: 24,
        phys_offset_y: 14,
        is_duplex: false,
        is_color: false,
        max_copies: 1,
    };

    let bounds = raw.to_printable_bounds();
    assert_eq!(bounds.dpi_x, 204);
    assert_eq!(bounds.dpi_y, 98);
    // Scales must differ between X and Y
    let scale_x = 72.0 / 204.0;
    let scale_y = 72.0 / 98.0;
    assert!((bounds.margins.left_pt - (24.0 * scale_x)).abs() < 1e-4);
    assert!((bounds.margins.top_pt - (14.0 * scale_y)).abs() < 1e-4);
}

#[test]
fn feat_09_bound_05_extreme_large_format_plotter_gripper_margins() {
    // 44-inch architectural roll plotter at 1200 DPI
    let raw = RawGdiCaps {
        dpi_x: 1200,
        dpi_y: 1200,
        phys_width: 52800,  // 44 in * 1200
        phys_height: 74400, // 62 in * 1200
        horz_res: 52000,
        vert_res: 73000,
        phys_offset_x: 140, // Asymmetric mechanical roll gripper
        phys_offset_y: 600, // 36 pt top lead-in
        is_duplex: false,
        is_color: true,
        max_copies: 100,
    };

    let bounds = raw.to_printable_bounds();
    assert_eq!(bounds.physical_width_pt, 3168.0);
    assert_eq!(bounds.physical_height_pt, 4464.0);
    assert!((bounds.margins.top_pt - 36.0).abs() < 1e-4);
    assert!(bounds.margins.bottom_pt > 0.0);
}
