//! Empirical Challenge Suite for Milestone 4 Iteration 2:
//! 1. Dynamic Registry OS Telemetry (live host verification & synthesis edge cases)
//! 2. Dynamic Disk & Spooler Queries (GetDiskFreeSpaceExW, quota & threshold boundaries)
//! 3. Diagnostic Serialization Integrity (JSON roundtrip, markdown injection & pipe/newline escaping)
//! 4. Presets Persistence & Group Clustering (CRUD, built-in non-deletion protection, cluster operations)

use std::collections::BTreeMap;

use pdfcraft_platform::diagnostics::{
    collect_diagnostic_report, evaluate_spool_disk_verdict,
    harvest_os_telemetry, harvest_spooler_telemetry, synthesize_windows_version,
    AppliedColorMode, AppliedOrientation, AppliedTicketTelemetry, DiagnosticReport,
    DriverGenerationKind, DuplexBindingKind, MarginsPt,
    PrinterCapabilitiesTelemetry, ResolutionDpi, SizePt, SpoolDiskVerdict,
    SpoolerServiceStatus, TicketDetails, TraceCategory, TraceLevel,
    TraceRecord,
};
use pdfcraft_platform::disk::{
    get_default_spool_directory, query_disk_space_bytes,
};

#[allow(dead_code)]
#[path = "../../ui-egui/src/workflows/print_presets.rs"]
mod print_presets;

use print_presets::{
    PresetColorMode, PresetDuplex, PresetFitMode, PresetOrientation, PrintJobPreset,
    PrintPresetStore, PrinterGroup,
};

// =========================================================================
// 1. DYNAMIC REGISTRY OS TELEMETRY
// =========================================================================

#[test]
fn challenge_live_host_windows_registry_telemetry() {
    let os = harvest_os_telemetry();

    // Verify basic platform facts
    assert_eq!(os.os_family, "windows");
    assert_eq!(os.architecture, "x86_64");
    assert!(os.cpu_cores >= 1, "cpu_cores must be >= 1, got {}", os.cpu_cores);
    assert_eq!(os.pointer_width, 64);

    // Verify that os_version and kernel_version are not dummy fallbacks
    assert_ne!(os.os_version, "Windows 10 Pro", "Must not return bare fallback string");
    assert_ne!(os.os_version, "Windows NT", "Must read genuine registry key");
    assert_ne!(os.kernel_version, "10.0", "Must have concrete build version");

    // Host machine runs build 26300 (Windows 11) with UBR 9974 and DisplayVersion 26H2
    assert!(
        os.os_version.contains("Windows 11"),
        "Build >= 22000 must be synthesized to Windows 11, got: '{}'",
        os.os_version
    );
    assert!(
        os.os_version.contains("26300"),
        "Must contain host build 26300, got: '{}'",
        os.os_version
    );
    assert!(
        os.os_version.contains("9974"),
        "Must contain host UBR 9974, got: '{}'",
        os.os_version
    );

    assert!(
        os.kernel_version.starts_with("10.0.26300"),
        "Kernel version must reflect NT build 10.0.26300, got: '{}'",
        os.kernel_version
    );
}

#[test]
fn challenge_windows_version_synthesis_matrix() {
    // Case 1: Windows 11 Build 22000+ reporting "Windows 10 Pro" in legacy registry key
    let s1 = synthesize_windows_version("Windows 10 Pro", "23H2", "22631", "3880");
    assert_eq!(s1, "Windows 11 Pro 23H2 (Build 22631.3880)");

    // Case 2: Windows 10 Build 19045 (legacy Windows 10 must NOT be altered to 11)
    let s2 = synthesize_windows_version("Windows 10 Enterprise", "22H2", "19045", "4291");
    assert_eq!(s2, "Windows 10 Enterprise 22H2 (Build 19045.4291)");

    // Case 3: Empty display version and empty UBR
    let s3 = synthesize_windows_version("Windows 10 Home", "", "22000", "");
    assert_eq!(s3, "Windows 11 Home (Build 22000)");

    // Case 4: Windows Server 2022 (should not touch server product names)
    let s4 = synthesize_windows_version("Windows Server 2022 Datacenter", "21H2", "20348", "1607");
    assert_eq!(s4, "Windows Server 2022 Datacenter 21H2 (Build 20348.1607)");

    // Case 5: Future Windows 11 build (e.g. 27000)
    let s5 = synthesize_windows_version("Windows 10 Pro", "25H2", "27500", "1000");
    assert_eq!(s5, "Windows 11 Pro 25H2 (Build 27500.1000)");
}

// =========================================================================
// 2. DYNAMIC DISK & SPOOLER QUERIES
// =========================================================================

#[test]
fn challenge_live_host_disk_space_queries() {
    let spool_dir = get_default_spool_directory();
    let spool_str = spool_dir.to_string_lossy();

    // Query spool directory
    let (avail_spool, total_spool) = query_disk_space_bytes(&spool_str);
    assert!(
        avail_spool > 0,
        "Available bytes on spool dir must be > 0, got {}",
        avail_spool
    );
    assert!(
        total_spool > 0,
        "Total bytes on spool dir must be > 0, got {}",
        total_spool
    );
    assert!(
        avail_spool <= total_spool,
        "Available bytes ({}) cannot exceed total bytes ({})",
        avail_spool,
        total_spool
    );

    // Query system drive C:\
    let (avail_c, total_c) = query_disk_space_bytes("C:\\");
    assert!(avail_c > 0, "C:\\ available bytes must be > 0");
    assert!(total_c > 0, "C:\\ total bytes must be > 0");
    assert!(avail_c <= total_c);

    // Query temp directory
    let temp_dir = std::env::temp_dir();
    let (avail_tmp, total_tmp) = query_disk_space_bytes(&temp_dir.to_string_lossy());
    assert!(avail_tmp > 0);
    assert!(total_tmp > 0);

    // Query invalid drive path: must not panic, must return (0, 0)
    let (avail_bad, total_bad) = query_disk_space_bytes("Z:\\NonExistent_Fake_Drive_98765\\");
    assert_eq!(avail_bad, 0);
    assert_eq!(total_bad, 0);
}

#[test]
fn challenge_spooler_telemetry_harvesting() {
    let spooler = harvest_spooler_telemetry();

    assert_eq!(spooler.service_name, "Spooler");
    assert_eq!(spooler.service_status, SpoolerServiceStatus::Running);
    assert!(
        !spooler.spool_directory.is_empty(),
        "Spool directory path must not be empty"
    );
    assert!(
        spooler.available_bytes > 0,
        "Harvested available bytes must be > 0"
    );
    assert!(
        spooler.total_bytes > 0,
        "Harvested total bytes must be > 0"
    );
    assert!(spooler.available_bytes <= spooler.total_bytes);

    // Verify verdict matches evaluation function
    let expected_verdict = evaluate_spool_disk_verdict(spooler.available_bytes);
    assert_eq!(spooler.disk_verdict, expected_verdict);
}

#[test]
fn challenge_disk_verdict_threshold_boundaries() {
    const CRITICAL: u64 = 250 * 1024 * 1024; // 250 MB
    const LOW: u64 = 1024 * 1024 * 1024;     // 1.0 GB

    // Critical exhaustion (< 250 MB)
    assert_eq!(evaluate_spool_disk_verdict(0), SpoolDiskVerdict::CriticalExhaustion);
    assert_eq!(evaluate_spool_disk_verdict(1), SpoolDiskVerdict::CriticalExhaustion);
    assert_eq!(evaluate_spool_disk_verdict(CRITICAL - 1), SpoolDiskVerdict::CriticalExhaustion);

    // Low space warning (250 MB <= bytes < 1.0 GB)
    assert_eq!(evaluate_spool_disk_verdict(CRITICAL), SpoolDiskVerdict::LowSpaceWarning);
    assert_eq!(evaluate_spool_disk_verdict(CRITICAL + 1), SpoolDiskVerdict::LowSpaceWarning);
    assert_eq!(evaluate_spool_disk_verdict(LOW - 1), SpoolDiskVerdict::LowSpaceWarning);

    // Sufficient (>= 1.0 GB)
    assert_eq!(evaluate_spool_disk_verdict(LOW), SpoolDiskVerdict::Sufficient);
    assert_eq!(evaluate_spool_disk_verdict(LOW + 1), SpoolDiskVerdict::Sufficient);
    assert_eq!(evaluate_spool_disk_verdict(500 * 1024 * 1024 * 1024), SpoolDiskVerdict::Sufficient);
    assert_eq!(evaluate_spool_disk_verdict(u64::MAX), SpoolDiskVerdict::Sufficient);
}

// =========================================================================
// 3. DIAGNOSTIC SERIALIZATION INTEGRITY
// =========================================================================

#[test]
fn challenge_diagnostic_adversarial_serialization_roundtrip() {
    let now = chrono::Utc::now();

    let adversarial_printer = PrinterCapabilitiesTelemetry {
        printer_name: "Office | Network | Laser (Color) & \"XPS\" <script>".to_string(),
        driver_name: "Vendor Driver \\ Special | Edition\r\nVersion 2".to_string(),
        driver_version: "12.34.56.78 | patch 4".to_string(),
        driver_generation: DriverGenerationKind::Win32V4PrintTicket,
        port_name: "TCP/IP: 192.168.1.50 | 9100".to_string(),
        is_default: true,
        is_color_supported: true,
        is_duplex_supported: true,
        supported_duplex_modes: vec![
            DuplexBindingKind::FlipLongEdge,
            DuplexBindingKind::FlipShortEdge,
        ],
        max_copies: 999,
        hardware_margins_pt: MarginsPt { left: 14.1, top: 14.1, right: 14.1, bottom: 14.1 },
        printable_area_pt: SizePt { width: 566.0, height: 790.0 },
        physical_sheet_pt: SizePt { width: 595.0, height: 842.0 },
        resolution_dpi: ResolutionDpi { dpi_x: 1200, dpi_y: 1200 },
        supported_paper_count: 8,
    };

    let adversarial_ticket = AppliedTicketTelemetry {
        printer_name: "Office | Network | Laser (Color) & \"XPS\" <script>".to_string(),
        generation: DriverGenerationKind::Win32V4PrintTicket,
        color_mode: AppliedColorMode::Color,
        duplex_mode: DuplexBindingKind::FlipLongEdge,
        orientation: AppliedOrientation::Portrait,
        paper_size_name: "ISO A4 | 210x297mm".to_string(),
        copies: 3,
        details: TicketDetails::PrintTicketV4 {
            feature_color: "psk:Color | RGB".to_string(),
            feature_duplex: "psk:TwoSidedLongEdge | LongEdge".to_string(),
            feature_orientation: "psk:Portrait".to_string(),
            feature_media_size: "psk:ISOA4".to_string(),
            raw_xml_snippet: Some("<psf:PrintTicket xmlns:psf=\"...\">...</psf:PrintTicket>".to_string()),
        },
    };

    let arabic_trace = TraceRecord {
        timestamp: now,
        level: TraceLevel::Info,
        category: TraceCategory::VectorFilter,
        printer_name: Some("طابعة المستندات الرئيسية".to_string()),
        message: "بدء تحويل المحتوى إلى K-only بالأبيض والأسود | صفحة 1 من 10\r\nتم بنجاح".to_string(),
        error_code: None,
        context: BTreeMap::new(),
    };

    let error_trace = TraceRecord {
        timestamp: now,
        level: TraceLevel::Error,
        category: TraceCategory::Spooler,
        printer_name: Some("Pipe|Printer".to_string()),
        message: "Spooling failed: Pipe | broken\nCheck network & spooler\r\nAccess Denied".to_string(),
        error_code: Some("0x80070005 | ERROR_ACCESS_DENIED".to_string()),
        context: BTreeMap::new(),
    };

    let report = collect_diagnostic_report(
        vec![adversarial_printer],
        vec![adversarial_ticket],
        vec![arabic_trace, error_trace],
    );

    // 1. Test pretty JSON serialization & deserialization
    let pretty_json = report.to_json_pretty().expect("Pretty JSON serialization must succeed");
    assert!(!pretty_json.is_empty());
    assert!(pretty_json.contains("Office | Network | Laser"));
    assert!(pretty_json.contains("طابعة المستندات الرئيسية"));

    let deserialized_pretty = DiagnosticReport::from_json(&pretty_json)
        .expect("Deserialization from pretty JSON must succeed");
    assert_eq!(report, deserialized_pretty);

    // 2. Test compact JSON serialization & deserialization
    let compact_json = report.to_json_compact().expect("Compact JSON serialization must succeed");
    assert!(!compact_json.is_empty());
    assert!(!compact_json.contains('\n'), "Compact JSON must be single-line");

    let deserialized_compact = DiagnosticReport::from_json(&compact_json)
        .expect("Deserialization from compact JSON must succeed");
    assert_eq!(report, deserialized_compact);

    // 3. Test Markdown table rendering and cell escaping
    let md = report.to_markdown_table();
    assert!(md.contains("# PdfCraft Diagnostic & Telemetry Report"));
    assert!(md.contains("## 1. Operating System & Environment"));
    assert!(md.contains("## 2. Spooler Subsystem Status"));
    assert!(md.contains("## 3. Printer Driver Capabilities"));
    assert!(md.contains("## 4. Applied Job Configuration"));
    assert!(md.contains("## 5. Execution Traces & Diagnostic Logs"));

    // Check table row structural integrity
    for (line_no, line) in md.lines().enumerate() {
        if line.starts_with('|') && line.ends_with('|') {
            // Count unescaped delimiter pipes
            let chars: Vec<char> = line.chars().collect();
            let mut unescaped_delimiters = 0;
            for i in 0..chars.len() {
                if chars[i] == '|' && (i == 0 || chars[i - 1] != '\\') {
                    unescaped_delimiters += 1;
                }
            }
            assert!(
                unescaped_delimiters >= 3,
                "Line {}: invalid markdown table row: '{}'",
                line_no,
                line
            );
        }
    }

    // Verify cell newline escaping to <br>
    assert!(
        md.contains("<br>Check network & spooler<br>Access Denied"),
        "Multi-line trace message must be converted to <br> tags in table cell"
    );
    assert!(
        md.contains("Pipe \\| broken"),
        "Raw pipe in trace message must be escaped with backslash"
    );
}

// =========================================================================
// 4. PRESETS PERSISTENCE & GROUP CLUSTERING
// =========================================================================

#[test]
fn challenge_preset_store_built_in_protection_and_crud() {
    let mut store = PrintPresetStore::with_defaults();

    // Verify initial defaults
    assert_eq!(store.presets.len(), 3);
    assert!(store.presets.contains_key("standard-duplex"));
    assert!(store.presets.contains_key("monochrome-eco"));
    assert!(store.presets.contains_key("engineering-100"));
    assert_eq!(store.active_preset_id.as_deref(), Some("standard-duplex"));

    // INVARIANT 1: Built-in presets CANNOT be deleted
    assert!(!store.delete_preset("standard-duplex"), "Deleting standard-duplex must fail");
    assert!(!store.delete_preset("monochrome-eco"), "Deleting monochrome-eco must fail");
    assert!(!store.delete_preset("engineering-100"), "Deleting engineering-100 must fail");
    assert_eq!(store.presets.len(), 3, "Presets count must remain 3");
    assert_eq!(store.active_preset_id.as_deref(), Some("standard-duplex"));

    // INVARIANT 2: User custom preset CRUD lifecycle
    let custom_preset = PrintJobPreset {
        id: "custom-confidential-tax".to_string(),
        name: "Confidential Tax Documents".to_string(),
        description: "Monochrome duplex secure print for finance".to_string(),
        is_built_in: false,
        copies: 5,
        color_mode: PresetColorMode::Monochrome,
        duplex: PresetDuplex::LongEdge,
        orientation: PresetOrientation::Portrait,
        fit_mode: PresetFitMode::FitToPrintable,
        custom_scale_pct: None,
        collate: true,
    };

    store.upsert_preset(custom_preset);
    assert_eq!(store.presets.len(), 4);
    assert!(store.presets.contains_key("custom-confidential-tax"));

    // Mutation via mutable reference
    if let Some(p) = store.get_preset_mut("custom-confidential-tax") {
        p.copies = 10;
        p.fit_mode = PresetFitMode::ShrinkOversized;
    }
    let p_check = store.get_preset("custom-confidential-tax").unwrap();
    assert_eq!(p_check.copies, 10);
    assert_eq!(p_check.fit_mode, PresetFitMode::ShrinkOversized);

    // Set active and verify active_preset()
    store.set_active_preset(Some("custom-confidential-tax".to_string()));
    assert_eq!(store.active_preset().unwrap().name, "Confidential Tax Documents");

    // Deletion of custom preset must succeed and clear active_preset_id
    let deleted = store.delete_preset("custom-confidential-tax");
    assert!(deleted, "User preset deletion must succeed");
    assert_eq!(store.presets.len(), 3);
    assert!(store.active_preset_id.is_none(), "Active preset ID must be reset to None on deletion");
    assert!(store.active_preset().is_none());

    // Deleting non-existent preset returns false
    assert!(!store.delete_preset("non_existent_preset_xyz"));
}

#[test]
fn challenge_printer_group_clustering_and_membership() {
    let mut store = PrintPresetStore::with_defaults();

    // Create a new cluster group
    let mut group = PrinterGroup::new("floor-3-fleet", "3rd Floor MFP Fleet", false);

    // Stress test: add 100 printers with duplicate additions
    for i in 0..100 {
        let name = format!("MFP-Floor3-{:03}", i);
        let inserted = group.add_printer(name.clone());
        assert!(inserted, "First insert of {} must succeed", name);

        // Duplicate insert: should return false and not grow set
        let dup = group.add_printer(name);
        assert!(!dup, "Duplicate insert must return false");
    }
    assert_eq!(group.printer_names.len(), 100);

    // Membership checks
    assert!(group.contains("MFP-Floor3-000"));
    assert!(group.contains("MFP-Floor3-099"));
    assert!(!group.contains("MFP-Floor3-100"));

    // Remove 20 printers
    for i in 0..20 {
        let name = format!("MFP-Floor3-{:03}", i);
        let removed = group.remove_printer(&name);
        assert!(removed, "Removal of {} must succeed", name);
        assert!(!group.contains(&name));
    }
    assert_eq!(group.printer_names.len(), 80);

    // Save to store
    store.upsert_group(group);
    assert_eq!(store.groups.len(), 1);
    store.set_active_group(Some("floor-3-fleet".to_string()));
    assert_eq!(store.active_group().unwrap().name, "3rd Floor MFP Fleet");

    // Built-in group deletion protection check
    let built_in_grp = PrinterGroup::new("built-in-cluster", "Default Factory Cluster", true);
    store.upsert_group(built_in_grp);
    assert_eq!(store.groups.len(), 2);

    let del_builtin = store.delete_group("built-in-cluster");
    assert!(!del_builtin, "Built-in group cannot be deleted");
    assert_eq!(store.groups.len(), 2);

    // Delete custom group: succeeds and clears active_group_id
    let del_custom = store.delete_group("floor-3-fleet");
    assert!(del_custom, "Custom group deletion must succeed");
    assert_eq!(store.groups.len(), 1);
    assert!(store.active_group_id.is_none());
}

#[test]
fn challenge_preset_store_file_io_persistence() {
    let mut store = PrintPresetStore::with_defaults();
    let mut grp = PrinterGroup::new("shipping-pack", "Shipping Label Thermal Cluster", false);
    grp.add_printer("Zebra-ZD621".to_string());
    grp.add_printer("Zebra-ZT411".to_string());
    store.upsert_group(grp);

    // Export to temporary file
    let temp_dir = std::env::temp_dir();
    let temp_file_path = temp_dir.join(format!("pdfcraft-presets-test-{}.json", std::process::id()));

    store.export_to_file(&temp_file_path).expect("Exporting presets store to file must succeed");
    assert!(temp_file_path.exists(), "Preset file must exist on disk");

    // Import from file
    let imported_store = PrintPresetStore::import_from_file(&temp_file_path)
        .expect("Importing presets store from file must succeed");

    assert_eq!(store, imported_store, "Imported store must match exported store exactly");

    // Clean up temporary test file
    let _ = std::fs::remove_file(&temp_file_path);
}
