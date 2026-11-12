//! Authoritative E2E Test Harness for the `pdfcraft` Production Print Studio.
//!
//! Provides hermetic, in-memory mock drivers, virtual spooler, stream inspection,
//! geometric margin verification, programmable fault injection, and performance probes.

pub mod benchmarks;
pub mod fault_injector;
pub mod margin_assertions;
pub mod mock_driver;
pub mod mock_spooler;
pub mod stream_inspector;

// Re-export core types and assertions for easy opaque-box consumption
pub use benchmarks::{
    BenchmarkAssertions, FluidityReport, MemoryProbe, MemorySnapshot, UiFluidityMonitor,
};
pub use fault_injector::{
    FaultAction, FaultAssertions, FaultAuditRecord, FaultInjector, FaultRule, FaultTrigger,
    InjectedFaultError,
};
pub use margin_assertions::{
    calculate_page_transform, ClippingExtent, ClippingExtentMm, MarginAssertions, PageFitMode,
    PageTransform,
};
pub use mock_driver::{
    create_mock_virtual_pdf_printer, CapturedIppJob, CapturedV3DevMode, DevModeValidationError,
    DriverGeneration, DuplexBinding, HardwareMargins, MockIppDriver, MockV3Driver,
    MockV3DriverConfig, MockV4Driver, MockV4DriverConfig, PrintJobSettings, PrintableBounds,
    PrinterCapabilities, DMCOLOR_COLOR, DMCOLOR_MONOCHROME, DMDUP_HORIZONTAL, DMDUP_SIMPLEX,
    DMDUP_VERTICAL, DMORIENT_LANDSCAPE, DMORIENT_PORTRAIT, DM_COLOR, DM_COPIES, DM_DUPLEX,
    DM_IN_BUFFER, DM_ORIENTATION, DM_OUT_BUFFER, IPP_STATUS_ERROR_ATTRIBUTES_OR_VALUES_NOT_SUPPORTED,
    IPP_STATUS_OK,
};
pub use mock_spooler::{
    CommittedSheetsLedger, DiskVerdict, MockDiskSpaceGovernor, MockSpooler, SheetStatus,
    SpoolDiskStatus,
};
pub use stream_inspector::{
    ColorSpaceContext, DefaultStreamInspector, InspectorError, KOnlyInspectionReport,
    PdfStreamInspector, StreamAssertions, StreamViolation,
};
