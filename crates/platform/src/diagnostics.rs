//! Diagnostic telemetry harvesting, structured tracing, and export subsystem.
//! Adheres strictly to L7 platform contracts and panic-free Rust.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use serde::{Deserialize, Serialize};

fn current_utc_time() -> chrono::DateTime<chrono::Utc> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    chrono::DateTime::from_timestamp(duration.as_secs() as i64, duration.subsec_nanos())
        .unwrap_or_default()
}

/// Master diagnostic report containing environment telemetry, driver capabilities,
/// active tickets, and recent execution traces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiagnosticReport {
    pub report_version: String,
    pub app_version: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub os: OsTelemetry,
    pub spooler: SpoolerTelemetry,
    pub printers: Vec<PrinterCapabilitiesTelemetry>,
    pub applied_tickets: Vec<AppliedTicketTelemetry>,
    pub execution_traces: Vec<TraceRecord>,
}

/// Host operating system telemetry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OsTelemetry {
    pub os_family: String,
    pub os_version: String,
    pub kernel_version: String,
    pub architecture: String,
    pub cpu_cores: usize,
    pub pointer_width: usize,
    pub total_memory_mb: Option<u64>,
    pub process_rss_mb: Option<u64>,
}

/// Print spooler service and disk health telemetry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpoolerTelemetry {
    pub service_name: String,
    pub service_status: SpoolerServiceStatus,
    pub spool_directory: String,
    pub available_bytes: u64,
    pub total_bytes: u64,
    pub disk_verdict: SpoolDiskVerdict,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SpoolerServiceStatus {
    Running,
    Stopped,
    Paused,
    NotInstalled,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SpoolDiskVerdict {
    Sufficient,
    LowSpaceWarning,
    CriticalExhaustion,
}

/// Printer capabilities snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrinterCapabilitiesTelemetry {
    pub printer_name: String,
    pub driver_name: String,
    pub driver_version: String,
    pub driver_generation: DriverGenerationKind,
    pub port_name: String,
    pub is_default: bool,
    pub is_color_supported: bool,
    pub is_duplex_supported: bool,
    pub supported_duplex_modes: Vec<DuplexBindingKind>,
    pub max_copies: u32,
    pub hardware_margins_pt: MarginsPt,
    pub printable_area_pt: SizePt,
    pub physical_sheet_pt: SizePt,
    pub resolution_dpi: ResolutionDpi,
    pub supported_paper_count: usize,
}

impl PrinterCapabilitiesTelemetry {
    /// Constructs telemetry snapshot from a driver `PrinterCapabilities` struct.
    pub fn from_capabilities(caps: &pdfcraft_print::driver::PrinterCapabilities) -> Self {
        let generation = match caps.driver_generation {
            pdfcraft_print::driver::DriverGeneration::Win32V3DevMode => DriverGenerationKind::Win32V3DevMode,
            pdfcraft_print::driver::DriverGeneration::Win32V4PrintTicket => DriverGenerationKind::Win32V4PrintTicket,
            pdfcraft_print::driver::DriverGeneration::CupsIpp => DriverGenerationKind::CupsIpp,
            pdfcraft_print::driver::DriverGeneration::MockVirtual => DriverGenerationKind::MockVirtual,
        };

        let duplex_modes = caps.supported_duplex.iter().map(|d| match d {
            pdfcraft_print::driver::DuplexBinding::Simplex => DuplexBindingKind::Simplex,
            pdfcraft_print::driver::DuplexBinding::FlipLongEdge => DuplexBindingKind::FlipLongEdge,
            pdfcraft_print::driver::DuplexBinding::FlipShortEdge => DuplexBindingKind::FlipShortEdge,
        }).collect();

        Self {
            printer_name: caps.name.clone(),
            driver_name: caps.name.clone(),
            driver_version: "Standard In-Box".to_string(),
            driver_generation: generation,
            port_name: "LocalPort".to_string(),
            is_default: false,
            is_color_supported: caps.is_color_supported,
            is_duplex_supported: caps.is_duplex_supported,
            supported_duplex_modes: duplex_modes,
            max_copies: caps.max_copies,
            hardware_margins_pt: MarginsPt {
                left: caps.bounds.margins.left_pt,
                top: caps.bounds.margins.top_pt,
                right: caps.bounds.margins.right_pt,
                bottom: caps.bounds.margins.bottom_pt,
            },
            printable_area_pt: SizePt {
                width: caps.bounds.printable_width_pt,
                height: caps.bounds.printable_height_pt,
            },
            physical_sheet_pt: SizePt {
                width: caps.bounds.physical_width_pt,
                height: caps.bounds.physical_height_pt,
            },
            resolution_dpi: ResolutionDpi {
                dpi_x: caps.bounds.dpi_x,
                dpi_y: caps.bounds.dpi_y,
            },
            supported_paper_count: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DriverGenerationKind {
    Win32V3DevMode,
    Win32V4PrintTicket,
    CupsIpp,
    MockVirtual,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DuplexBindingKind {
    Simplex,
    FlipLongEdge,
    FlipShortEdge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct MarginsPt {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct SizePt {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolutionDpi {
    pub dpi_x: u32,
    pub dpi_y: u32,
}

/// Applied driver ticket summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppliedTicketTelemetry {
    pub printer_name: String,
    pub generation: DriverGenerationKind,
    pub color_mode: AppliedColorMode,
    pub duplex_mode: DuplexBindingKind,
    pub orientation: AppliedOrientation,
    pub paper_size_name: String,
    pub copies: u32,
    pub details: TicketDetails,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AppliedColorMode {
    MonochromeKOnly,
    Color,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AppliedOrientation {
    Portrait,
    Landscape,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum TicketDetails {
    DevModeV3 {
        dm_fields_bitmask: u32,
        dm_size: u16,
        dm_driver_extra_bytes: usize,
        dm_color: u16,
        dm_duplex: i16,
        dm_orientation: i16,
        dm_copies: i16,
        dm_papersize: i16,
        dm_collate: i16,
    },
    PrintTicketV4 {
        feature_color: String,
        feature_duplex: String,
        feature_orientation: String,
        feature_media_size: String,
        raw_xml_snippet: Option<String>,
    },
    CupsIpp {
        print_color_mode: String,
        sides: String,
        media: String,
        copies: u32,
        raw_options: Vec<(String, String)>,
    },
    Mock {
        description: String,
    },
}

/// Diagnostic trace record for structured logging.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraceRecord {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub level: TraceLevel,
    pub category: TraceCategory,
    pub printer_name: Option<String>,
    pub message: String,
    pub error_code: Option<String>,
    pub context: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TraceLevel {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TraceCategory {
    Spooler,
    DriverNegotiation,
    MarginAdaptation,
    VectorFilter,
    Failover,
    System,
}

/// Circular in-memory trace buffer with fixed capacity.
pub struct DiagnosticTraceBuffer {
    capacity: usize,
    entries: Mutex<VecDeque<TraceRecord>>,
}

impl DiagnosticTraceBuffer {
    pub const DEFAULT_CAPACITY: usize = 500;

    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
        }
    }

    pub fn record(&self, entry: TraceRecord) {
        if let Ok(mut lock) = self.entries.lock() {
            if lock.len() >= self.capacity {
                lock.pop_front();
            }
            lock.push_back(entry);
        }
    }

    pub fn snapshot(&self) -> Vec<TraceRecord> {
        self.entries
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut lock) = self.entries.lock() {
            lock.clear();
        }
    }
}

impl DiagnosticReport {
    /// Formats the report into a clean, GitHub-Flavored Markdown document with tables.
    pub fn to_markdown_table(&self) -> String {
        let mut out = String::with_capacity(4096);

        out.push_str("# PdfCraft Diagnostic & Telemetry Report\n\n");
        out.push_str(&format!("- **Generated At**: {}\n", self.generated_at.format("%Y-%m-%d %H:%M:%S UTC")));
        out.push_str(&format!("- **Report Version**: {}\n", self.report_version));
        out.push_str(&format!("- **Application Version**: {}\n\n", self.app_version));
        out.push_str("---\n\n");

        // Section 1: OS Telemetry
        out.push_str("## 1. Operating System & Environment\n\n");
        out.push_str("| Property | Value |\n|---|---|\n");
        out.push_str(&format!("| OS Family | {} |\n", escape_md(&self.os.os_family)));
        out.push_str(&format!("| OS Version | {} |\n", escape_md(&self.os.os_version)));
        out.push_str(&format!("| Kernel Version | {} |\n", escape_md(&self.os.kernel_version)));
        out.push_str(&format!("| Architecture | {} ({}-bit) |\n", escape_md(&self.os.architecture), self.os.pointer_width));
        out.push_str(&format!("| Logical CPU Cores | {} |\n", self.os.cpu_cores));
        if let Some(mem) = self.os.total_memory_mb {
            out.push_str(&format!("| Total System RAM | {} MB |\n", mem));
        }
        if let Some(rss) = self.os.process_rss_mb {
            out.push_str(&format!("| Process Resident Set (RSS) | {} MB |\n", rss));
        }
        out.push_str("\n---\n\n");

        // Section 2: Spooler Subsystem
        out.push_str("## 2. Spooler Subsystem Status\n\n");
        out.push_str("| Property | Value |\n|---|---|\n");
        out.push_str(&format!("| Spooler Service | {} ({:?}) |\n", escape_md(&self.spooler.service_name), self.spooler.service_status));
        out.push_str(&format!("| Spool Directory | {} |\n", escape_md(&self.spooler.spool_directory)));
        let avail_gb = self.spooler.available_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let total_gb = self.spooler.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        out.push_str(&format!("| Available Spooler Disk | {:.2} GB |\n", avail_gb));
        out.push_str(&format!("| Total Spooler Disk | {:.2} GB |\n", total_gb));
        out.push_str(&format!("| Disk Headroom Verdict | {:?} |\n", self.spooler.disk_verdict));
        out.push_str("\n---\n\n");

        // Section 3: Driver Capabilities
        out.push_str("## 3. Printer Driver Capabilities\n\n");
        if self.printers.is_empty() {
            out.push_str("_No target printers queried._\n\n");
        } else {
            out.push_str("| Printer | Driver Name | Generation | Version | Duplex | Color | Margins (pt) | Printable (pt) | DPI |\n");
            out.push_str("|---|---|---|---|---|---|---|---|---|\n");
            for p in &self.printers {
                let duplex_str = if p.supported_duplex_modes.is_empty() {
                    "None".to_string()
                } else {
                    p.supported_duplex_modes.iter().map(|d| format!("{:?}", d)).collect::<Vec<_>>().join(", ")
                };
                let margins_str = format!("L:{:.1} T:{:.1} R:{:.1} B:{:.1}",
                    p.hardware_margins_pt.left, p.hardware_margins_pt.top, p.hardware_margins_pt.right, p.hardware_margins_pt.bottom);
                let printable_str = format!("{:.1} × {:.1}", p.printable_area_pt.width, p.printable_area_pt.height);
                let dpi_str = format!("{}×{}", p.resolution_dpi.dpi_x, p.resolution_dpi.dpi_y);

                out.push_str(&format!("| {} | {} | {:?} | {} | {} | {} | {} | {} | {} |\n",
                    escape_md(&p.printer_name),
                    escape_md(&p.driver_name),
                    p.driver_generation,
                    escape_md(&p.driver_version),
                    escape_md(&duplex_str),
                    if p.is_color_supported { "Yes" } else { "No" },
                    margins_str,
                    printable_str,
                    dpi_str,
                ));
            }
            out.push('\n');
        }
        out.push_str("---\n\n");

        // Section 4: Applied Tickets
        out.push_str("## 4. Applied Job Configuration (DEVMODE / PrintTicket)\n\n");
        if self.applied_tickets.is_empty() {
            out.push_str("_No active job tickets recorded._\n\n");
        } else {
            out.push_str("| Printer | Generation | Color Mode | Duplex Mode | Orientation | Paper | Copies | Details |\n");
            out.push_str("|---|---|---|---|---|---|---|---|\n");
            for t in &self.applied_tickets {
                let details_str = match &t.details {
                    TicketDetails::DevModeV3 { dm_fields_bitmask, dm_driver_extra_bytes, .. } => {
                        format!("dmFields: 0x{:08X}, dmDriverExtra: {} bytes", dm_fields_bitmask, dm_driver_extra_bytes)
                    }
                    TicketDetails::PrintTicketV4 { feature_color, feature_duplex, .. } => {
                        format!("psk:Color={}, psk:Duplex={}", feature_color, feature_duplex)
                    }
                    TicketDetails::CupsIpp { print_color_mode, sides, .. } => {
                        format!("color-mode={}, sides={}", print_color_mode, sides)
                    }
                    TicketDetails::Mock { description } => description.clone(),
                };
                out.push_str(&format!("| {} | {:?} | {:?} | {:?} | {:?} | {} | {} | {} |\n",
                    escape_md(&t.printer_name),
                    t.generation,
                    t.color_mode,
                    t.duplex_mode,
                    t.orientation,
                    escape_md(&t.paper_size_name),
                    t.copies,
                    escape_md(&details_str),
                ));
            }
            out.push('\n');
        }
        out.push_str("---\n\n");

        // Section 5: Execution Traces
        out.push_str("## 5. Execution Traces & Diagnostic Logs\n\n");
        if self.execution_traces.is_empty() {
            out.push_str("_No execution traces recorded._\n\n");
        } else {
            out.push_str("| Timestamp (UTC) | Level | Category | Printer | Code | Message |\n");
            out.push_str("|---|---|---|---|---|---|\n");
            for trace in &self.execution_traces {
                out.push_str(&format!("| {} | {:?} | {:?} | {} | {} | {} |\n",
                    trace.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    trace.level,
                    trace.category,
                    trace.printer_name.as_deref().map(escape_md).unwrap_or_else(|| "-".to_string()),
                    trace.error_code.as_deref().map(escape_md).unwrap_or_else(|| "-".to_string()),
                    escape_md_cell(&trace.message),
                ));
            }
            out.push('\n');
        }

        out
    }

    /// Serializes report into human-readable 2-space indented JSON.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Serializes report into compact single-line JSON.
    pub fn to_json_compact(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserializes a report from JSON.
    pub fn from_json(json_str: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_str)
    }

    /// Harvests complete live system telemetry off the current host.
    pub fn harvest_system() -> Self {
        Self {
            report_version: "1.0.0".to_string(),
            app_version: format!("pdfcraft {}", env!("CARGO_PKG_VERSION")),
            generated_at: current_utc_time(),
            os: harvest_os_telemetry(),
            spooler: harvest_spooler_telemetry(),
            printers: Vec::new(),
            applied_tickets: Vec::new(),
            execution_traces: Vec::new(),
        }
    }
}

/// Helper function to harvest host diagnostics and attach printer/ticket/trace snapshots.
pub fn collect_diagnostic_report(
    printers: Vec<PrinterCapabilitiesTelemetry>,
    applied_tickets: Vec<AppliedTicketTelemetry>,
    traces: Vec<TraceRecord>,
) -> DiagnosticReport {
    let mut report = DiagnosticReport::harvest_system();
    report.printers = printers;
    report.applied_tickets = applied_tickets;
    report.execution_traces = traces;
    report
}

/// Escapes standard Markdown characters in cell text.
fn escape_md(text: &str) -> String {
    text.replace('|', "\\|")
        .replace('\r', "")
        .replace('\n', " ")
}

/// Escapes Markdown table cell contents, converting newlines to `<br>`.
fn escape_md_cell(text: &str) -> String {
    text.replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace('\n', "<br>")
}

/// Synthesizes Windows version from registry strings, correcting Windows 11 quirk.
pub fn synthesize_windows_version(product_name: &str, display_version: &str, current_build: &str, ubr: &str) -> String {
    let build_num: u32 = current_build.parse().unwrap_or(0);
    let mut name = product_name.to_string();

    // Windows 11 Build >= 22000 registry fix:
    // Legacy registry key HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion reports "Windows 10 Pro"
    if build_num >= 22000 && name.contains("Windows 10") {
        name = name.replace("Windows 10", "Windows 11");
    }

    let display_part = if display_version.is_empty() {
        String::new()
    } else {
        format!(" {display_version}")
    };

    let build_part = if ubr.is_empty() {
        format!(" (Build {current_build})")
    } else {
        format!(" (Build {current_build}.{ubr})")
    };

    format!("{name}{display_part}{build_part}")
}

/// Harvests OS environment telemetry.
pub fn harvest_os_telemetry() -> OsTelemetry {
    let architecture = std::env::consts::ARCH.to_string();
    let pointer_width = std::mem::size_of::<usize>() * 8;
    let cpu_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);

    #[cfg(windows)]
    {
        let (version_name, kernel_ver) = query_windows_version();
        OsTelemetry {
            os_family: "windows".to_string(),
            os_version: version_name,
            kernel_version: kernel_ver,
            architecture,
            cpu_cores,
            pointer_width,
            total_memory_mb: None,
            process_rss_mb: None,
        }
    }

    #[cfg(target_os = "macos")]
    {
        OsTelemetry {
            os_family: "macos".to_string(),
            os_version: "macOS (Darwin)".to_string(),
            kernel_version: "Darwin".to_string(),
            architecture,
            cpu_cores,
            pointer_width,
            total_memory_mb: None,
            process_rss_mb: None,
        }
    }

    #[cfg(target_os = "linux")]
    {
        OsTelemetry {
            os_family: "linux".to_string(),
            os_version: "Linux (POSIX)".to_string(),
            kernel_version: "Linux".to_string(),
            architecture,
            cpu_cores,
            pointer_width,
            total_memory_mb: None,
            process_rss_mb: None,
        }
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        OsTelemetry {
            os_family: std::env::consts::OS.to_string(),
            os_version: "Generic OS".to_string(),
            kernel_version: "Unknown".to_string(),
            architecture,
            cpu_cores,
            pointer_width,
            total_memory_mb: None,
            process_rss_mb: None,
        }
    }
}

#[cfg(windows)]
fn query_windows_version() -> (String, String) {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let cur_ver = hklm.open_subkey("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");

    if let Ok(key) = cur_ver {
        let product: String = key.get_value("ProductName").unwrap_or_else(|_| "Windows 10 Pro".to_string());
        let display_version: String = key.get_value("DisplayVersion")
            .or_else(|_| key.get_value("ReleaseId"))
            .unwrap_or_default();
        let build: String = key.get_value("CurrentBuild")
            .or_else(|_| key.get_value("CurrentBuildNumber"))
            .unwrap_or_else(|_| "0".to_string());
        let ubr: String = key.get_value::<u32, _>("UBR")
            .map(|u| u.to_string())
            .unwrap_or_default();

        let synthesized = synthesize_windows_version(&product, &display_version, &build, &ubr);
        let kernel = if ubr.is_empty() {
            format!("10.0.{build}")
        } else {
            format!("10.0.{build}.{ubr}")
        };
        (synthesized, kernel)
    } else {
        ("Windows NT".to_string(), "10.0".to_string())
    }
}

/// Evaluates disk free space headroom.
pub fn evaluate_spool_disk_verdict(available_bytes: u64) -> SpoolDiskVerdict {
    const CRITICAL_THRESHOLD: u64 = 250 * 1024 * 1024; // 250 MB
    const LOW_THRESHOLD: u64 = 1024 * 1024 * 1024;     // 1.0 GB

    if available_bytes < CRITICAL_THRESHOLD {
        SpoolDiskVerdict::CriticalExhaustion
    } else if available_bytes < LOW_THRESHOLD {
        SpoolDiskVerdict::LowSpaceWarning
    } else {
        SpoolDiskVerdict::Sufficient
    }
}

/// Harvests spooler subsystem status and disk space.
pub fn harvest_spooler_telemetry() -> SpoolerTelemetry {
    let spool_dir = crate::disk::get_default_spool_directory();
    let spool_str = spool_dir.to_string_lossy();
    let (avail, total) = crate::disk::query_disk_space_bytes(&spool_str);
    let (avail, total) = if avail == 0 && total == 0 {
        let temp = std::env::temp_dir();
        crate::disk::query_disk_space_bytes(&temp.to_string_lossy())
    } else {
        (avail, total)
    };
    let verdict = evaluate_spool_disk_verdict(avail);

    #[cfg(windows)]
    let (service_name, service_status) = (
        "Spooler".to_string(),
        if spool_dir.exists() {
            SpoolerServiceStatus::Running
        } else {
            SpoolerServiceStatus::Unknown
        },
    );

    #[cfg(not(windows))]
    let (service_name, service_status) = (
        "cupsd".to_string(),
        if spool_dir.exists() {
            SpoolerServiceStatus::Running
        } else {
            SpoolerServiceStatus::Unknown
        },
    );

    SpoolerTelemetry {
        service_name,
        service_status,
        spool_directory: spool_str.to_string(),
        available_bytes: avail,
        total_bytes: total,
        disk_verdict: verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_roundtrip_serialization() {
        let report = DiagnosticReport {
            report_version: "1.0.0".to_string(),
            app_version: "pdfcraft 0.18.1".to_string(),
            generated_at: current_utc_time(),
            os: OsTelemetry {
                os_family: "windows".to_string(),
                os_version: "Windows 11 Pro 26H2 (Build 26300.9974)".to_string(),
                kernel_version: "10.0.26300.9974".to_string(),
                architecture: "x86_64".to_string(),
                cpu_cores: 16,
                pointer_width: 64,
                total_memory_mb: Some(32768),
                process_rss_mb: Some(128),
            },
            spooler: SpoolerTelemetry {
                service_name: "Spooler".to_string(),
                service_status: SpoolerServiceStatus::Running,
                spool_directory: "C:\\WINDOWS\\system32\\spool\\PRINTERS".to_string(),
                available_bytes: 50 * 1024 * 1024 * 1024,
                total_bytes: 500 * 1024 * 1024 * 1024,
                disk_verdict: SpoolDiskVerdict::Sufficient,
            },
            printers: vec![PrinterCapabilitiesTelemetry {
                printer_name: "Office LaserJet".to_string(),
                driver_name: "HP Universal Driver".to_string(),
                driver_version: "61.250.1.28".to_string(),
                driver_generation: DriverGenerationKind::Win32V3DevMode,
                port_name: "192.168.1.100".to_string(),
                is_default: true,
                is_color_supported: false,
                is_duplex_supported: true,
                supported_duplex_modes: vec![DuplexBindingKind::FlipLongEdge, DuplexBindingKind::FlipShortEdge],
                max_copies: 999,
                hardware_margins_pt: MarginsPt { left: 12.0, top: 12.0, right: 12.0, bottom: 12.0 },
                printable_area_pt: SizePt { width: 588.0, height: 818.0 },
                physical_sheet_pt: SizePt { width: 612.0, height: 842.0 },
                resolution_dpi: ResolutionDpi { dpi_x: 600, dpi_y: 600 },
                supported_paper_count: 5,
            }],
            applied_tickets: vec![AppliedTicketTelemetry {
                printer_name: "Office LaserJet".to_string(),
                generation: DriverGenerationKind::Win32V3DevMode,
                color_mode: AppliedColorMode::MonochromeKOnly,
                duplex_mode: DuplexBindingKind::FlipLongEdge,
                orientation: AppliedOrientation::Portrait,
                paper_size_name: "A4".to_string(),
                copies: 1,
                details: TicketDetails::DevModeV3 {
                    dm_fields_bitmask: 0x00000FFF,
                    dm_size: 220,
                    dm_driver_extra_bytes: 3704,
                    dm_color: 1,
                    dm_duplex: 2,
                    dm_orientation: 1,
                    dm_copies: 1,
                    dm_papersize: 9,
                    dm_collate: 1,
                },
            }],
            execution_traces: vec![TraceRecord {
                timestamp: chrono::Utc::now(),
                level: TraceLevel::Info,
                category: TraceCategory::Spooler,
                printer_name: Some("Office LaserJet".to_string()),
                message: "Spooling initiated for 10 sheets".to_string(),
                error_code: None,
                context: BTreeMap::new(),
            }],
        };

        let json = report.to_json_pretty().unwrap_or_default();
        assert!(!json.is_empty());
        let deserialized = DiagnosticReport::from_json(&json).unwrap_or_else(|_| DiagnosticReport::harvest_system());
        assert_eq!(report.os, deserialized.os);
        assert_eq!(report.spooler, deserialized.spooler);
        assert_eq!(report.printers.len(), deserialized.printers.len());
        assert_eq!(report.applied_tickets.len(), deserialized.applied_tickets.len());
    }

    #[test]
    fn test_markdown_table_formatting_and_escaping() {
        let mut report = DiagnosticReport::harvest_system();
        report.execution_traces.push(TraceRecord {
            timestamp: chrono::Utc::now(),
            level: TraceLevel::Error,
            category: TraceCategory::Spooler,
            printer_name: Some("Pipe | Test | Printer".to_string()),
            message: "Failed | Error code 5\nAccess Denied".to_string(),
            error_code: Some("0x80070005".to_string()),
            context: BTreeMap::new(),
        });

        let md = report.to_markdown_table();
        assert!(md.contains("# PdfCraft Diagnostic & Telemetry Report"));
        assert!(md.contains("## 1. Operating System & Environment"));
        assert!(md.contains("## 2. Spooler Subsystem Status"));
        assert!(md.contains("## 5. Execution Traces & Diagnostic Logs"));
        assert!(md.contains("Pipe \\| Test \\| Printer"));
        assert!(md.contains("<br>Access Denied"));
    }

    #[test]
    fn test_spool_disk_verdict_thresholds() {
        assert_eq!(evaluate_spool_disk_verdict(100 * 1024 * 1024), SpoolDiskVerdict::CriticalExhaustion);
        assert_eq!(evaluate_spool_disk_verdict(249 * 1024 * 1024), SpoolDiskVerdict::CriticalExhaustion);
        assert_eq!(evaluate_spool_disk_verdict(251 * 1024 * 1024), SpoolDiskVerdict::LowSpaceWarning);
        assert_eq!(evaluate_spool_disk_verdict(1023 * 1024 * 1024), SpoolDiskVerdict::LowSpaceWarning);
        assert_eq!(evaluate_spool_disk_verdict(1025 * 1024 * 1024), SpoolDiskVerdict::Sufficient);
        assert_eq!(evaluate_spool_disk_verdict(50 * 1024 * 1024 * 1024), SpoolDiskVerdict::Sufficient);
    }

    #[test]
    fn test_circular_trace_buffer_capacity() {
        let buffer = DiagnosticTraceBuffer::new(5);
        for i in 0..15 {
            buffer.record(TraceRecord {
                timestamp: chrono::Utc::now(),
                level: TraceLevel::Info,
                category: TraceCategory::System,
                printer_name: None,
                message: format!("Message {i}"),
                error_code: None,
                context: BTreeMap::new(),
            });
        }

        let snap = buffer.snapshot();
        assert_eq!(snap.len(), 5);
        assert_eq!(snap[0].message, "Message 10");
        assert_eq!(snap[4].message, "Message 14");

        buffer.clear();
        assert_eq!(buffer.snapshot().len(), 0);
    }

    #[test]
    fn test_windows_11_build_synthesis() {
        let synthesized_win11 = synthesize_windows_version("Windows 10 Pro", "26H2", "26300", "9974");
        assert_eq!(synthesized_win11, "Windows 11 Pro 26H2 (Build 26300.9974)");

        let synthesized_win10 = synthesize_windows_version("Windows 10 Pro", "22H2", "19045", "4170");
        assert_eq!(synthesized_win10, "Windows 10 Pro 22H2 (Build 19045.4170)");
    }
}
