//! crates/platform/tests/driver_m2_challenge.rs
//!
//! Empirical Challenge Test Suite for Milestone 2:
//! 1. MockVirtualDriver failure injection modes
//! 2. CUPS IPP 2.0 message serializer (RFC 8010/8011 binary frame verification)
//! 3. Win32 V4 Delta PrintTicket XML generation

use pdfcraft_platform::cups::{CupsDriverAdapter, IppMessageWriter, IppOperation, IppTag};
use pdfcraft_platform::mock::{MockDriverConfig, MockVirtualDriver};
use pdfcraft_print::driver::{
    ClippingExtent, ColorMode, DriverError, DriverGeneration, DuplexBinding,
    PageFitMode, PreFlightSeverity, PrintDriverAdapter, PrintJobSettings,
};
use std::time::{Duration, Instant};

#[test]
fn test_mock_driver_query_capabilities_success_and_failure() {
    // 1. Successful query
    let config = MockDriverConfig {
        printer_name: "Office-LaserJet-Pro".to_string(),
        is_duplex_supported: true,
        supported_duplex: vec![
            DuplexBinding::Simplex,
            DuplexBinding::FlipLongEdge,
            DuplexBinding::FlipShortEdge,
        ],
        is_color_supported: true,
        max_copies: 500,
        ..Default::default()
    };

    let driver = MockVirtualDriver::new(config.clone());
    let caps = driver
        .query_capabilities("Office-LaserJet-Pro")
        .expect("should query capabilities successfully");

    assert_eq!(caps.name, "Office-LaserJet-Pro");
    assert_eq!(caps.driver_generation, DriverGeneration::MockVirtual);
    assert!(caps.is_duplex_supported);
    assert!(caps.is_color_supported);
    assert_eq!(caps.max_copies, 500);

    // 2. Injected failure on query_capabilities
    let mut fail_config = config;
    fail_config.fail_query_capabilities = Some(DriverError::CapabilitiesQueryFailed(
        "Simulated printer hardware communication timeout".to_string(),
    ));

    let failing_driver = MockVirtualDriver::new(fail_config);
    let err = failing_driver
        .query_capabilities("Office-LaserJet-Pro")
        .expect_err("should return injected error");

    match err {
        DriverError::CapabilitiesQueryFailed(msg) => {
            assert!(msg.contains("communication timeout"));
        }
        other => panic!("Unexpected error type: {other:?}"),
    }
}

#[test]
fn test_mock_driver_validate_job_failure_injection() {
    let office_driver = MockVirtualDriver::office_duplex_color("Office-Color-MFP");
    let caps = office_driver
        .query_capabilities("Office-Color-MFP")
        .expect("query caps");

    let settings = PrintJobSettings {
        job_title: "Audit Report".to_string(),
        printer_name: "Office-Color-MFP".to_string(),
        copies: 1,
        color_mode: ColorMode::Color,
        duplex_mode: DuplexBinding::FlipLongEdge,
        fit_mode: PageFitMode::FitPrintableArea,
        page_bounds: (595.2, 841.68),
        landscape: false,
        paper_name: None,
    };

    // Baseline validation passes
    let report = office_driver.validate_job(&caps, &settings);
    assert!(report.is_valid_for_dispatch);
    assert_eq!(report.issues, vec![PreFlightSeverity::Ok]);

    // Injected Critical Mismatch failure
    let cfg_critical = MockDriverConfig {
        printer_name: "Office-Color-MFP".to_string(),
        fail_validate_job: Some(PreFlightSeverity::CriticalMismatch(
            "Vendor tray locked by another user".to_string(),
        )),
        ..Default::default()
    };
    let failing_validator = MockVirtualDriver::new(cfg_critical);

    let critical_report = failing_validator.validate_job(&caps, &settings);
    assert!(!critical_report.is_valid_for_dispatch);
    assert!(critical_report.has_critical_mismatch());
    assert!(critical_report.issues.iter().any(|i| match i {
        PreFlightSeverity::CriticalMismatch(msg) => msg.contains("Vendor tray locked"),
        _ => false,
    }));

    // Injected Warning Clipping failure
    let clip_extent = ClippingExtent {
        left_pt: 14.5,
        top_pt: 0.0,
        right_pt: 12.0,
        bottom_pt: 0.0,
    };
    let cfg_clipping = MockDriverConfig {
        printer_name: "Office-Color-MFP".to_string(),
        fail_validate_job: Some(PreFlightSeverity::WarningClipping(clip_extent)),
        ..Default::default()
    };
    let clipping_validator = MockVirtualDriver::new(cfg_clipping);

    let clip_report = clipping_validator.validate_job(&caps, &settings);
    assert!(clip_report.is_valid_for_dispatch);
    assert!(clip_report.has_clipping_hazard());
    assert!(clip_report.issues.iter().any(|i| match i {
        PreFlightSeverity::WarningClipping(c) => (c.left_pt - 14.5).abs() < 1e-4,
        _ => false,
    }));
}

#[test]
fn test_mock_driver_spool_job_immediate_failure_injection() {
    let driver = MockVirtualDriver::failing(
        "Dead-Printer",
        DriverError::SpoolFailed("Printer offline: paper jam in finisher".to_string()),
    );

    let settings = PrintJobSettings::default();
    let dummy_pdf = b"%PDF-1.7 mock document stream";

    let res = driver.spool_job(&settings, dummy_pdf);
    assert!(res.is_err());
    match res.unwrap_err() {
        DriverError::SpoolFailed(msg) => {
            assert!(msg.contains("paper jam"));
        }
        other => panic!("Unexpected error: {other:?}"),
    }

    // Verify no jobs were recorded in ledger
    assert!(driver.spooled_jobs().is_empty());
    assert!(driver.last_job().is_none());
}

#[test]
fn test_mock_driver_mid_job_failure_after_sheets_and_reset() {
    let cfg = MockDriverConfig {
        printer_name: "Mid-Job-Failing-Printer".to_string(),
        fail_after_sheets: Some(2), // Fails after sheet 2 (on sheet 3)
        ..Default::default()
    };

    let driver = MockVirtualDriver::new(cfg);
    let settings = PrintJobSettings::default();
    let sheet1_data = b"%PDF-sheet-1";
    let sheet2_data = b"%PDF-sheet-2";
    let sheet3_data = b"%PDF-sheet-3";

    // Sheet 1: Should succeed
    let res1 = driver.spool_job(&settings, sheet1_data);
    assert!(res1.is_ok());
    let summary1 = res1.expect("sheet 1 ok");
    assert_eq!(summary1.job_id, 1);
    assert_eq!(summary1.pages_spooled, 1);

    // Sheet 2: Should succeed
    let res2 = driver.spool_job(&settings, sheet2_data);
    assert!(res2.is_ok());
    let summary2 = res2.expect("sheet 2 ok");
    assert_eq!(summary2.job_id, 2);

    // Sheet 3: Exceeds limit (counter becomes 3 > 2) -> Must fail!
    let res3 = driver.spool_job(&settings, sheet3_data);
    assert!(res3.is_err());
    match res3.unwrap_err() {
        DriverError::SpoolFailed(msg) => {
            assert!(msg.contains("Simulated printer failure after 2 sheets"));
        }
        other => panic!("Expected SpoolFailed, got: {other:?}"),
    }

    // Verify ledger contains exactly 2 successfully spooled sheets
    let jobs = driver.spooled_jobs();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].pdf_bytes, sheet1_data);
    assert_eq!(jobs[1].pdf_bytes, sheet2_data);

    // Verify reset clears the failure counter and ledger
    driver.reset();
    assert_eq!(driver.spooled_jobs().len(), 0);

    // Spooling sheet 1 again should now succeed
    let res_after_reset = driver.spool_job(&settings, sheet1_data);
    assert!(res_after_reset.is_ok());
    assert_eq!(driver.spooled_jobs().len(), 1);
}

#[test]
fn test_mock_driver_spool_delay_per_sheet_backpressure() {
    let delay = Duration::from_millis(50);
    let cfg = MockDriverConfig {
        printer_name: "Throttled-Virtual-Printer".to_string(),
        spool_delay_per_sheet: Some(delay),
        ..Default::default()
    };

    let driver = MockVirtualDriver::new(cfg);
    let settings = PrintJobSettings::default();
    let dummy_pdf = b"%PDF-throttled-stream";

    let start = Instant::now();
    let res = driver.spool_job(&settings, dummy_pdf);
    let elapsed = start.elapsed();

    assert!(res.is_ok());
    assert!(
        elapsed >= Duration::from_millis(40),
        "Expected spool delay of at least 40ms, got {elapsed:?}"
    );
    assert_eq!(driver.spooled_jobs().len(), 1);
}

#[test]
fn test_cups_ipp_message_writer_binary_packet_layout() {
    let mut writer = IppMessageWriter::new(IppOperation::PrintJob, 42);

    // Group 1: Operation Attributes (0x01)
    writer.start_group(IppTag::OperationAttributes);
    writer.add_attribute_str(IppTag::Charset, "attributes-charset", "utf-8");
    writer.add_attribute_str(IppTag::NaturalLanguage, "attributes-natural-language", "en-US");
    writer.add_attribute_str(
        IppTag::Uri,
        "printer-uri",
        "ipp://localhost:631/printers/EPSON",
    );

    // Group 2: Job Attributes (0x02)
    writer.start_group(IppTag::JobAttributes);
    writer.add_attribute_str(IppTag::Keyword, "sides", "two-sided-long-edge");
    writer.add_attribute_i32(IppTag::Integer, "copies", 3);

    // End of Attributes (0x03)
    writer.end_attributes();
    let packet = writer.finish();

    // Verification of binary wire protocol (RFC 8010 section 3.1.1):
    // 1. IPP Version 2.0 (0x0200)
    assert_eq!(packet[0], 0x02);
    assert_eq!(packet[1], 0x00);

    // 2. Operation ID PrintJob (0x0002)
    assert_eq!(packet[2], 0x00);
    assert_eq!(packet[3], 0x02);

    // 3. Request ID 42 (0x0000002A)
    assert_eq!(&packet[4..8], &42u32.to_be_bytes());

    // 4. Operation Attributes delimiter tag (0x01)
    assert_eq!(packet[8], 0x01);

    // 5. First attribute: attributes-charset (tag 0x47)
    let mut idx = 9;
    assert_eq!(packet[idx], IppTag::Charset as u8);
    idx += 1;
    let name_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + name_len]).unwrap(),
        "attributes-charset"
    );
    idx += name_len;
    let val_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + val_len]).unwrap(),
        "utf-8"
    );
    idx += val_len;

    // 6. Natural Language attribute (tag 0x48)
    assert_eq!(packet[idx], IppTag::NaturalLanguage as u8);
    idx += 1;
    let name_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + name_len]).unwrap(),
        "attributes-natural-language"
    );
    idx += name_len;
    let val_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + val_len]).unwrap(),
        "en-US"
    );
    idx += val_len;

    // 7. Uri attribute (tag 0x45)
    assert_eq!(packet[idx], IppTag::Uri as u8);
    idx += 1;
    let name_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + name_len]).unwrap(),
        "printer-uri"
    );
    idx += name_len;
    let val_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + val_len]).unwrap(),
        "ipp://localhost:631/printers/EPSON"
    );
    idx += val_len;

    // 8. Job Attributes delimiter tag (0x02)
    assert_eq!(packet[idx], 0x02);
    idx += 1;

    // 9. sides attribute (tag 0x44 Keyword)
    assert_eq!(packet[idx], IppTag::Keyword as u8);
    idx += 1;
    let name_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + name_len]).unwrap(),
        "sides"
    );
    idx += name_len;
    let val_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + val_len]).unwrap(),
        "two-sided-long-edge"
    );
    idx += val_len;

    // 10. copies attribute (tag 0x21 Integer, 4 bytes value)
    assert_eq!(packet[idx], IppTag::Integer as u8);
    idx += 1;
    let name_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(
        std::str::from_utf8(&packet[idx..idx + name_len]).unwrap(),
        "copies"
    );
    idx += name_len;
    let val_len = u16::from_be_bytes([packet[idx], packet[idx + 1]]) as usize;
    idx += 2;
    assert_eq!(val_len, 4);
    let int_val = i32::from_be_bytes([
        packet[idx],
        packet[idx + 1],
        packet[idx + 2],
        packet[idx + 3],
    ]);
    assert_eq!(int_val, 3);
    idx += 4;

    // 11. End of Attributes tag (0x03)
    assert_eq!(packet[idx], 0x03);
    assert_eq!(idx + 1, packet.len());
}

#[test]
fn test_cups_driver_adapter_build_print_job_request_flags() {
    let adapter = CupsDriverAdapter::with_host_port("cups.corp.internal", 631);

    // Test Duplex FlipShortEdge and ColorMode Monochrome
    let settings = PrintJobSettings {
        job_title: "Financial Ledger".to_string(),
        printer_name: "Floor2-ColorLaser".to_string(),
        copies: 5,
        color_mode: ColorMode::MonochromeKOnly,
        duplex_mode: DuplexBinding::FlipShortEdge,
        fit_mode: PageFitMode::FitPrintableArea,
        page_bounds: (595.2, 841.68),
        landscape: false,
        paper_name: None,
    };

    let req = adapter.build_print_job_request("Floor2-ColorLaser", &settings);

    // Verify version 0x0200 and operation PrintJob 0x0002
    assert_eq!(&req[0..4], &[0x02, 0x00, 0x00, 0x02]);

    let req_str = String::from_utf8_lossy(&req);
    assert!(req_str.contains("attributes-charset"));
    assert!(req_str.contains("utf-8"));
    assert!(req_str.contains("ipp://cups.corp.internal:631/printers/Floor2-ColorLaser"));
    assert!(req_str.contains("Financial Ledger"));
    assert!(req_str.contains("application/pdf"));
    assert!(req_str.contains("two-sided-short-edge"));
    assert!(req_str.contains("monochrome"));

    // Verify last byte is 0x03 (end of attributes)
    assert_eq!(*req.last().unwrap(), 0x03);
}

#[cfg(windows)]
#[test]
fn test_win32_v4_delta_print_ticket_xml_generation() {
    use pdfcraft_platform::win32_v4::generate_delta_print_ticket_xml;

    // Scenario A: Monochrome, FlipLongEdge, Landscape, Copies = 4, Paper = ISOA4
    let xml_a = generate_delta_print_ticket_xml(
        ColorMode::MonochromeKOnly,
        DuplexBinding::FlipLongEdge,
        true, // landscape
        4,    // copies
        Some("ISOA4"),
    );

    // 1. XML Header and root namespace declarations
    assert!(xml_a.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(xml_a.contains("<psf:PrintTicket version=\"1\""));
    assert!(xml_a.contains("xmlns:psf=\"http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework\""));
    assert!(xml_a.contains("xmlns:psk=\"http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords\""));
    assert!(xml_a.ends_with("</psf:PrintTicket>"));

    // 2. Feature: psk:PageOutputColor
    assert!(xml_a.contains("<psf:Feature name=\"psk:PageOutputColor\">"));
    assert!(xml_a.contains("<psf:Option name=\"psk:Monochrome\" />"));

    // 3. Feature: psk:JobDuplexAllDocumentsContiguously
    assert!(xml_a.contains("<psf:Feature name=\"psk:JobDuplexAllDocumentsContiguously\">"));
    assert!(xml_a.contains("<psf:Option name=\"psk:TwoSidedLongEdge\" />"));

    // 4. Feature: psk:PageOrientation
    assert!(xml_a.contains("<psf:Feature name=\"psk:PageOrientation\">"));
    assert!(xml_a.contains("<psf:Option name=\"psk:Landscape\" />"));

    // 5. Feature: psk:PageMediaSize
    assert!(xml_a.contains("<psf:Feature name=\"psk:PageMediaSize\">"));
    assert!(xml_a.contains("<psf:Option name=\"psk:ISOA4\" />"));

    // 6. ParameterInit: psk:JobCopiesAllDocuments
    assert!(xml_a.contains("<psf:ParameterInit name=\"psk:JobCopiesAllDocuments\">"));
    assert!(xml_a.contains("<psf:Value xsi:type=\"xsd:integer\">4</psf:Value>"));

    // Scenario B: Color, Simplex, Portrait, Copies = 1 (no parameter init), No paper
    let xml_b = generate_delta_print_ticket_xml(
        ColorMode::Color,
        DuplexBinding::Simplex,
        false, // portrait
        1,     // copies = 1 (default)
        None,
    );

    assert!(xml_b.contains("<psf:Option name=\"psk:Color\" />"));
    assert!(xml_b.contains("<psf:Option name=\"psk:OneSided\" />"));
    assert!(xml_b.contains("<psf:Option name=\"psk:Portrait\" />"));
    assert!(!xml_b.contains("psk:PageMediaSize"));
    assert!(!xml_b.contains("psk:JobCopiesAllDocuments"));

    // Scenario C: Duplex FlipShortEdge
    let xml_c = generate_delta_print_ticket_xml(
        ColorMode::Color,
        DuplexBinding::FlipShortEdge,
        false,
        1,
        None,
    );
    assert!(xml_c.contains("<psf:Option name=\"psk:TwoSidedShortEdge\" />"));
}
