//! Multi-printer print orchestration coordinator with isolated fault boundaries,
//! per-printer ledger tracking, stream health monitoring, and one-click failover rerouting.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use pdfcraft_print::governor::{
    ConcurrencyGovernor, GovernorError, PrinterJobEstimate, SheetRasterMode,
    calculate_required_spool_bytes,
};
use pdfcraft_print::{Settings, Sheet, layout};

/// Stall detection timeout: if an actively spooling stream receives no progress updates for this
/// duration, it transitions to `StreamHealth::Stalled`.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Low-bandwidth threshold in KB/s below which an active stream is marked as `StreamHealth::Throttled`.
pub const THROTTLE_THRESHOLD_KBPS: f64 = 25.0;

/// Shared read-only PDF document bytes backing concurrent print workers.
pub use crate::snapshot::SharedPdfSource;

/// Cooperative cancellation token shared across supervisor, workers, governor, and UI.
pub use pdfcraft_print::governor::CancellationToken;

/// Background builder closure producing a print-ready PDF for `Settings`.
pub type PreparedPdfBuilderFn =
    Arc<dyn Fn(&Settings) -> Result<Vec<u8>, String> + Send + Sync>;

/// Thread-safe snapshot of an open document's printable state, captured in O(1) on the UI thread
/// so heavy imposition, decryption, form/comment synthesis, or rasterization can execute on a
/// background worker thread without blocking UI frames.
#[derive(Clone)]
pub struct PreparedPrintSource {
    builder: PreparedPdfBuilderFn,
}

impl PreparedPrintSource {
    /// Wraps a thread-safe print-ready PDF builder closure.
    pub fn from_builder<F>(builder: F) -> Self
    where
        F: Fn(&Settings) -> Result<Vec<u8>, String> + Send + Sync + 'static,
    {
        Self {
            builder: Arc::new(builder),
        }
    }

    /// Builds the print-ready imposed PDF for `settings` safely on any background worker thread.
    pub fn build_pdf(&self, settings: &Settings) -> Result<Vec<u8>, String> {
        (self.builder)(settings)
    }
}

/// Analytical spool disk footprint estimator for a multi-printer batch.
pub fn estimate_batch_spool_bytes(
    total_sheets: usize,
    jobs: &[&pdfcraft_print::spool::Job],
    is_raster: bool,
) -> u64 {
    let mode = if is_raster {
        SheetRasterMode::Raster300Dpi
    } else {
        SheetRasterMode::Vector
    };
    let estimates: Vec<PrinterJobEstimate> = jobs
        .iter()
        .map(|j| PrinterJobEstimate::new(total_sheets, j.copies.max(1) as usize, mode))
        .collect();
    calculate_required_spool_bytes(&estimates)
}

/// Checks whether `name` is a virtual file-saving or fax printer unsuitable for unattended failover.
fn is_virtual_save_queue(name: &str) -> bool {
    if pdfcraft_print::is_virtual_save_printer(name) {
        return true;
    }
    let lower = name.trim().to_ascii_lowercase();
    lower.contains("microsoft print to pdf")
        || lower.contains("xps document writer")
        || lower.contains("onenote")
        || lower == "fax"
        || lower.ends_with(" fax")
}

/// Per-printer ledger tracking sheets committed to the spooler vs. sheets pending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedSheetsLedger {
    /// Total sheets to be printed for this printer.
    pub total_sheets: usize,
    /// Indices of sheets confirmed by the spooler (0-indexed).
    pub committed_sheets: Vec<usize>,
    /// Indices of sheets still pending transmission.
    pub pending_sheets: Vec<usize>,
    /// Index of the sheet that failed, if any.
    pub failed_sheet: Option<usize>,
}

impl CommittedSheetsLedger {
    /// Creates a new ledger for `total_sheets`, with all sheets initially pending.
    pub fn new(total_sheets: usize) -> Self {
        Self {
            total_sheets,
            committed_sheets: Vec::new(),
            pending_sheets: (0..total_sheets).collect(),
            failed_sheet: None,
        }
    }

    /// Creates a ledger from an explicit list of pending sheet indices (used during failover).
    pub fn from_pending_indices(total_in_batch: usize, pending: &[usize]) -> Self {
        Self {
            total_sheets: total_in_batch,
            committed_sheets: Vec::new(),
            pending_sheets: pending.to_vec(),
            failed_sheet: None,
        }
    }

    /// Marks sheet `sheet_idx` as committed, removing it from pending.
    pub fn mark_committed(&mut self, sheet_idx: usize) {
        if let Some(pos) = self.pending_sheets.iter().position(|&s| s == sheet_idx) {
            self.pending_sheets.remove(pos);
        }
        if !self.committed_sheets.contains(&sheet_idx) {
            self.committed_sheets.push(sheet_idx);
            self.committed_sheets.sort_unstable();
        }
    }

    /// Marks all sheets from 0 up to `count - 1` as committed.
    pub fn mark_committed_up_to(&mut self, count: usize) {
        let count = count.min(self.total_sheets);
        self.committed_sheets.clear();
        for i in 0..count {
            self.committed_sheets.push(i);
        }
        self.pending_sheets.retain(|&s| s >= count);
    }

    /// Records a failure on the given sheet index.
    pub fn record_failure(&mut self, sheet_idx: usize) {
        self.failed_sheet = Some(sheet_idx);
        self.committed_sheets.retain(|&s| s != sheet_idx);
        if !self.pending_sheets.contains(&sheet_idx) && sheet_idx < self.total_sheets {
            self.pending_sheets.insert(0, sheet_idx);
            self.pending_sheets.sort_unstable();
            self.pending_sheets.dedup();
        }
    }

    /// Whether all sheets have been committed.
    pub fn is_complete(&self) -> bool {
        self.committed_sheets.len() >= self.total_sheets && self.pending_sheets.is_empty()
    }

    /// Slices the remaining uncommitted sheets for failover rerouting.
    pub fn slice_for_failover(&self) -> Vec<usize> {
        self.pending_sheets.clone()
    }

    pub fn committed_count(&self) -> usize {
        self.committed_sheets.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending_sheets.len()
    }
}

/// Dynamic health status of an active printer spool stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamHealth {
    /// Healthy active transfer with measured throughput.
    Healthy {
        rate_kbps: f64,
        sheets_per_sec: f64,
    },
    /// Progressing but constrained by spooler/driver backpressure or low throughput.
    Throttled {
        rate_kbps: f64,
        reason: String,
    },
    /// No progress updates received within `STALL_TIMEOUT` during active transmission.
    Stalled {
        stalled_duration_secs: u64,
    },
    /// Driver or hardware error occurred.
    Faulted {
        error: String,
        recoverable: bool,
    },
    /// Spooling completed successfully.
    Completed {
        total_sheets: usize,
        duration_secs: f64,
    },
}

/// Comprehensive telemetry snapshot for a single printer stream.
#[derive(Debug, Clone)]
pub struct PrinterStreamTelemetry {
    pub printer_name: String,
    pub current_sheet: usize,
    pub total_sheets: usize,
    pub bytes_sent: u64,
    pub health: StreamHealth,
    pub last_progress_time: Instant,
    pub ledger: CommittedSheetsLedger,
    pub error_message: Option<String>,
}

/// Commands accepted by the coordinator.
#[derive(Debug, Clone)]
pub enum CoordinatorCommand {
    StartBatch {
        printers: Vec<String>,
        settings: Settings,
    },
    CancelPrinter(String),
    CancelAll,
    FailoverReroute {
        from_printer: String,
        to_printer: String,
    },
}

/// Events emitted by background printer workers to the coordinator and UI.
#[derive(Debug, Clone)]
pub enum CoordinatorEvent {
    BatchStarted {
        total_printers: usize,
    },
    ProgressUpdate(PrinterStreamTelemetry),
    StreamStalled {
        printer_name: String,
        stalled_duration_secs: u64,
    },
    PrinterCompleted {
        printer_name: String,
        total_sheets: usize,
        duration_secs: f64,
    },
    PrinterFailed {
        printer_name: String,
        error: String,
        committed_sheets: Vec<usize>,
        pending_sheets: Vec<usize>,
        suggested_failover: Option<String>,
    },
    PrinterCancelled {
        printer_name: String,
    },
    FailoverDispatched {
        from_printer: String,
        to_printer: String,
        rerouted_sheets: usize,
    },
    BatchFinished {
        successful: usize,
        failed: usize,
        cancelled: usize,
    },
}

/// Failover error conditions.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum FailoverError {
    #[error("source printer '{0}' not found in job batch")]
    PrinterNotFound(String),
    #[error("printer '{0}' has no pending sheets to reroute")]
    NoPendingSheets(String),
    #[error("target printer '{0}' is already active in current batch")]
    TargetPrinterBusy(String),
    #[error("failed to layout pages for target printer '{0}': {1}")]
    LayoutError(String, String),
    #[error("job batch has been cancelled")]
    BatchCancelled,
}

/// Coordinator initialization errors.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PrintCoordinatorError {
    #[error("no printers selected for dispatch")]
    NoPrintersSelected,
    #[error("layout error: {0}")]
    Layout(String),
    #[error("failed to spawn printer worker thread for '{0}': {1}")]
    ThreadSpawn(String, String),
}

/// Custom spooler submission callback used for hermetic testing or custom transport hooks.
pub type SpoolSubmitFn = Arc<
    dyn Fn(&[u8], &pdfcraft_print::spool::Job, &mut dyn FnMut(usize, usize)) -> Result<String, String>
        + Send
        + Sync,
>;

/// Wakeup callback invoked whenever a `CoordinatorEvent` is emitted (e.g. `egui::Context::request_repaint`).
pub type EventWakeupFn = Arc<dyn Fn() + Send + Sync>;

/// Full configuration for dispatching a unified print batch across one or multiple printers.
pub struct BatchDispatchConfig {
    pub printers: Vec<String>,
    pub settings: Settings,
    pub doc_source: SharedPdfSource,
    pub page_sizes: Vec<(f64, f64)>,
    /// Per-printer pre-configured `spool::Job` (copies, collate, duplex, grayscale, paper media, CUPS orientation, title).
    pub printer_jobs: HashMap<String, pdfcraft_print::spool::Job>,
    /// Fallback template job used for any printer not explicitly in `printer_jobs` (including failover targets).
    pub default_job: pdfcraft_print::spool::Job,
    /// Optional pre-imposed print-ready PDF bytes (e.g. from `Session::print_pdf`, preserving unsaved edits, form fields, annotations, and layers).
    pub pre_imposed_pdf: Option<Arc<Vec<u8>>>,
    /// Optional custom spooler hook (defaults to `None` in production so OS `gdi_driver` and `two_sided` run).
    pub custom_spooler: Option<SpoolSubmitFn>,
    /// Optional UI wakeup callback invoked whenever a worker emits an event, decoupled from `custom_spooler`.
    pub on_event_wakeup: Option<EventWakeupFn>,
}

/// Event channel paired with an optional wakeup callback.
#[derive(Clone)]
struct EventNotifier {
    tx: Sender<CoordinatorEvent>,
    wakeup: Option<EventWakeupFn>,
}

impl EventNotifier {
    fn send(&self, event: CoordinatorEvent) {
        let _ = self.tx.send(event);
        if let Some(ref wakeup) = self.wakeup {
            wakeup();
        }
    }
}

/// Zero-copy per-batch spool adaptation cache (`OnceLock`-memoized across concurrent workers).
struct BatchSpoolBuffer {
    base: Arc<Vec<u8>>,
    color_tiled: OnceLock<Arc<Vec<u8>>>,
    color_flattened: OnceLock<Arc<Vec<u8>>>,
    mono_tiled: OnceLock<Option<Arc<Vec<u8>>>>,
    mono_flattened: OnceLock<Option<Arc<Vec<u8>>>>,
}

impl BatchSpoolBuffer {
    fn new(base: Arc<Vec<u8>>) -> Self {
        Self {
            base,
            color_tiled: OnceLock::new(),
            color_flattened: OnceLock::new(),
            mono_tiled: OnceLock::new(),
            mono_flattened: OnceLock::new(),
        }
    }

    fn color_ready(&self) -> &Arc<Vec<u8>> {
        self.color_tiled.get_or_init(|| {
            if !cfg!(windows) {
                return self.base.clone();
            }
            let mut probe = pdfcraft_print::spool::Job::default();
            Arc::new(crate::spooler::spooler_copy(
                self.base.as_ref().clone(),
                &mut probe,
                false,
            ))
        })
    }

    fn mono_ready(&self) -> Option<&Arc<Vec<u8>>> {
        self.mono_tiled
            .get_or_init(|| {
                let mut probe = pdfcraft_print::spool::Job {
                    grayscale: true,
                    grey_pages: false,
                    ..Default::default()
                };
                let stage1 = crate::spooler::spooler_copy(
                    self.base.as_ref().clone(),
                    &mut probe,
                    false,
                );
                let filtered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    pdfcraft_print::monochrome::MonochromeVectorFilter::filter_document_to_k_only(
                        &stage1,
                    )
                    .ok()
                }))
                .ok()
                .flatten();
                match filtered {
                    Some(bytes) => Some(Arc::new(bytes)),
                    None if probe.grey_pages => Some(Arc::new(stage1)),
                    None => None,
                }
            })
            .as_ref()
    }

    fn for_job(&self, job: &mut pdfcraft_print::spool::Job, flatten_transparency: bool) -> Arc<Vec<u8>> {
        if job.grayscale {
            if flatten_transparency && cfg!(windows) {
                let flat_opt = self.mono_flattened.get_or_init(|| {
                    let mono = self.mono_ready()?.clone();
                    let mut probe = pdfcraft_print::spool::Job {
                        grayscale: true,
                        grey_pages: true,
                        ..Default::default()
                    };
                    Some(Arc::new(crate::spooler::spooler_copy(
                        mono.as_ref().clone(),
                        &mut probe,
                        true,
                    )))
                });
                if let Some(buf) = flat_opt {
                    job.grey_pages = true;
                    return buf.clone();
                }
            } else if let Some(mono) = self.mono_ready() {
                job.grey_pages = true;
                return mono.clone();
            }
        }

        if flatten_transparency && cfg!(windows) {
            return self
                .color_flattened
                .get_or_init(|| {
                    let mut probe = pdfcraft_print::spool::Job::default();
                    Arc::new(crate::spooler::spooler_copy(
                        self.base.as_ref().clone(),
                        &mut probe,
                        true,
                    ))
                })
                .clone();
        }

        self.color_ready().clone()
    }

    fn slice_sheets(
        &self,
        sheet_indices: &[usize],
        job: &mut pdfcraft_print::spool::Job,
        flatten_transparency: bool,
    ) -> Result<Arc<Vec<u8>>, String> {
        let (source_arc, grey_converted) = if job.grayscale {
            match self.mono_ready() {
                Some(mono) => (mono.clone(), true),
                None => (self.color_ready().clone(), false),
            }
        } else {
            (self.color_ready().clone(), false)
        };

        let doc = pdfcraft_cos::Document::open(source_arc)
            .map_err(|e| format!("Failed to open imposed PDF for failover slice: {e}"))?;
        let total = pdfcraft_model::pages(&doc).len();
        if sheet_indices.len() == total && sheet_indices.iter().copied().eq(0..total) {
            return Ok(self.for_job(job, flatten_transparency));
        }

        let sliced_doc = pdfcraft_organize::extract_pages(&doc, sheet_indices)
            .map_err(|e| format!("Failed to slice remaining imposed sheets: {e}"))?;
        let mut sliced_bytes =
            pdfcraft_cos::write_full(&sliced_doc, &pdfcraft_cos::SaveOptions::default())
                .map_err(|e| format!("Failed to serialize sliced failover PDF: {e}"))?;

        if grey_converted {
            job.grey_pages = true;
        }
        if flatten_transparency && cfg!(windows) {
            let mut probe = pdfcraft_print::spool::Job {
                grayscale: job.grayscale,
                grey_pages: true,
                ..Default::default()
            };
            sliced_bytes = crate::spooler::spooler_copy(sliced_bytes, &mut probe, true);
        }
        Ok(Arc::new(sliced_bytes))
    }
}

/// Shared lazy preparation cache for an entire print batch.
/// Ensures imposition, unified grayscale/K-only conversion, large-image tiling, and GDI flattening
/// happen at most once across all workers and remain available for zero-reimposition failover.
struct BatchPreparedCache {
    doc_source: SharedPdfSource,
    prepared_source: Option<PreparedPrintSource>,
    settings: Settings,
    governor: Arc<ConcurrencyGovernor>,
    spool_buffer: OnceLock<Result<Arc<BatchSpoolBuffer>, String>>,
}

impl BatchPreparedCache {
    fn new(
        doc_source: SharedPdfSource,
        prepared_source: Option<PreparedPrintSource>,
        pre_imposed_pdf: Option<Arc<Vec<u8>>>,
        settings: Settings,
        governor: Arc<ConcurrencyGovernor>,
    ) -> Self {
        let spool_buffer = OnceLock::new();
        if let Some(imposed) = pre_imposed_pdf {
            let _ = spool_buffer.set(Ok(Arc::new(BatchSpoolBuffer::new(imposed))));
        }
        Self {
            doc_source,
            prepared_source,
            settings,
            governor,
            spool_buffer,
        }
    }

    fn resolve(&self, cancel_token: &CancellationToken) -> Result<Arc<BatchSpoolBuffer>, String> {
        if let Some(existing) = self.spool_buffer.get() {
            return existing.clone();
        }
        let _cpu_permit = self
            .governor
            .acquire_cpu_permit(cancel_token)
            .map_err(governor_err_string)?;
        self.spool_buffer
            .get_or_init(|| self.build_imposed_buffer())
            .clone()
    }

    fn build_imposed_buffer(&self) -> Result<Arc<BatchSpoolBuffer>, String> {
        if let Some(ref prepared) = self.prepared_source {
            let imposed = prepared
                .build_pdf(&self.settings)
                .map_err(|e| format!("Print preparation failure: {e}"))?;
            return Ok(Arc::new(BatchSpoolBuffer::new(Arc::new(imposed))));
        }

        let doc_arc = match &self.doc_source {
            #[cfg(not(target_arch = "wasm32"))]
            SharedPdfSource::Mmap(m) => Arc::new(m.as_ref().to_vec()),
            SharedPdfSource::Heap(h) => h.clone(),
        };
        let cos_doc = pdfcraft_cos::Document::open(doc_arc)
            .map_err(|e| format!("Failed to open PDF document: {e}"))?;
        let imposed = pdfcraft_print::impose(&cos_doc, &self.settings)
            .map_err(|e| format!("Imposition failure: {e}"))?;
        Ok(Arc::new(BatchSpoolBuffer::new(Arc::new(imposed))))
    }
}

fn governor_err_string(err: GovernorError) -> String {
    match err {
        GovernorError::Cancelled => "Cancelled by operator".to_string(),
        other => other.to_string(),
    }
}

/// Control structure for an individual worker.
struct WorkerControl {
    cancel_token: CancellationToken,
    spooling_active: Arc<AtomicBool>,
}

/// Control handle for an active multi-printer print batch.
pub struct MultiJobHandle {
    pub batch_id: u64,
    workers: HashMap<String, WorkerControl>,
    event_rx: Receiver<CoordinatorEvent>,
    notifier: EventNotifier,
    telemetry: HashMap<String, PrinterStreamTelemetry>,
    master_cancel: CancellationToken,
    batch_cache: Arc<BatchPreparedCache>,
    batch_sheets: Vec<Sheet>,
    worker_sheet_map: HashMap<String, Vec<usize>>,
    worker_duplex: HashMap<String, bool>,
    printer_jobs: HashMap<String, pdfcraft_print::spool::Job>,
    default_job: pdfcraft_print::spool::Job,
    custom_spooler: Option<SpoolSubmitFn>,
}

impl MultiJobHandle {
    /// Non-blocking event poll called on the UI thread (<0.2ms per frame).
    /// Drains available events, updates internal telemetry, and checks `STALL_TIMEOUT` only on
    /// streams that have actively entered spool transmission.
    pub fn poll_events(&mut self) -> Vec<CoordinatorEvent> {
        let mut received = Vec::new();
        let mut drained = 0;
        while drained < 64 {
            match self.event_rx.try_recv() {
                Ok(event) => {
                    self.update_telemetry_from_event(&event);
                    received.push(event);
                    drained += 1;
                }
                Err(_) => break,
            }
        }

        let now = Instant::now();
        for (name, tel) in self.telemetry.iter_mut() {
            let is_active = self
                .workers
                .get(name)
                .is_some_and(|w| w.spooling_active.load(Ordering::Acquire));
            if is_active
                && matches!(
                    tel.health,
                    StreamHealth::Healthy { .. } | StreamHealth::Throttled { .. }
                )
            {
                let elapsed = now.duration_since(tel.last_progress_time);
                if elapsed >= STALL_TIMEOUT {
                    tel.health = StreamHealth::Stalled {
                        stalled_duration_secs: elapsed.as_secs(),
                    };
                    received.push(CoordinatorEvent::StreamStalled {
                        printer_name: name.clone(),
                        stalled_duration_secs: elapsed.as_secs(),
                    });
                }
            }
        }

        received
    }

    fn update_telemetry_from_event(&mut self, event: &CoordinatorEvent) {
        match event {
            CoordinatorEvent::ProgressUpdate(tel) => {
                self.telemetry.insert(tel.printer_name.clone(), tel.clone());
            }
            CoordinatorEvent::PrinterCompleted {
                printer_name,
                total_sheets,
                duration_secs,
            } => {
                if let Some(tel) = self.telemetry.get_mut(printer_name) {
                    tel.health = StreamHealth::Completed {
                        total_sheets: *total_sheets,
                        duration_secs: *duration_secs,
                    };
                    tel.ledger.mark_committed_up_to(*total_sheets);
                }
            }
            CoordinatorEvent::PrinterFailed {
                printer_name,
                error,
                committed_sheets,
                pending_sheets,
                ..
            } => {
                if let Some(tel) = self.telemetry.get_mut(printer_name) {
                    tel.health = StreamHealth::Faulted {
                        error: error.clone(),
                        recoverable: !pending_sheets.is_empty(),
                    };
                    tel.error_message = Some(error.clone());
                    tel.ledger.committed_sheets = committed_sheets.clone();
                    tel.ledger.pending_sheets = pending_sheets.clone();
                }
            }
            CoordinatorEvent::PrinterCancelled { printer_name } => {
                if let Some(tel) = self.telemetry.get_mut(printer_name) {
                    tel.health = StreamHealth::Faulted {
                        error: "Cancelled by operator".to_string(),
                        recoverable: false,
                    };
                }
            }
            _ => {}
        }
    }

    /// Selects the first non-busy, non-virtual backup printer from `candidates` for failover.
    pub fn suggest_failover_target(&self, from_printer: &str, candidates: &[String]) -> Option<String> {
        candidates
            .iter()
            .find(|candidate| {
                let name = candidate.as_str();
                if name == from_printer || is_virtual_save_queue(name) {
                    return false;
                }
                !self.telemetry.get(name).is_some_and(|tel| {
                    matches!(
                        tel.health,
                        StreamHealth::Healthy { .. }
                            | StreamHealth::Throttled { .. }
                            | StreamHealth::Stalled { .. }
                    )
                })
            })
            .cloned()
    }

    /// Gets current telemetry for a printer.
    pub fn get_telemetry(&self, printer_name: &str) -> Option<&PrinterStreamTelemetry> {
        self.telemetry.get(printer_name)
    }

    /// Gets current telemetry for a printer (compatibility accessor).
    pub fn telemetry(&self, printer_name: &str) -> Option<&PrinterStreamTelemetry> {
        self.get_telemetry(printer_name)
    }

    /// Returns true if all printers in the batch have reached a terminal state (compatibility accessor).
    pub fn is_complete(&self) -> bool {
        self.is_all_completed()
    }

    /// Returns a reference to the ledger for a printer (compatibility accessor).
    pub fn ledger(&self, printer_name: &str) -> Option<&CommittedSheetsLedger> {
        self.get_telemetry(printer_name).map(|t| &t.ledger)
    }

    /// Gets telemetry map for all printers in this batch.
    pub fn all_telemetry(&self) -> &HashMap<String, PrinterStreamTelemetry> {
        &self.telemetry
    }

    /// Signals cooperative cancellation for a single printer.
    pub fn cancel_printer(&mut self, printer_name: &str) -> bool {
        if let Some(worker) = self.workers.get(printer_name) {
            worker.cancel_token.cancel();
            worker.spooling_active.store(false, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Signals cooperative cancellation for all printers in this batch.
    pub fn cancel_all(&mut self) {
        self.master_cancel.cancel();
        for worker in self.workers.values() {
            worker.cancel_token.cancel();
            worker.spooling_active.store(false, Ordering::Release);
        }
    }

    /// Returns true if all printers in the batch have reached a terminal state (Completed, Faulted).
    pub fn is_all_completed(&self) -> bool {
        !self.telemetry.is_empty()
            && self.telemetry.values().all(|t| {
                matches!(
                    t.health,
                    StreamHealth::Completed { .. } | StreamHealth::Faulted { .. }
                )
            })
    }

    /// Computes global imposed sheet indices to reroute from `from_printer`, aligning duplex jobs
    /// to even physical leaf boundaries `(first_pending / 2) * 2` so front/back pairing never shifts.
    fn compute_failover_global_sheets(&self, from_printer: &str, pending_local: &[usize], local_total: usize) -> Vec<usize> {
        let is_duplex = self.worker_duplex.get(from_printer).copied().unwrap_or(false);
        let first_pending = pending_local.first().copied().unwrap_or(local_total);
        let resume_local = if is_duplex {
            (first_pending / 2) * 2
        } else {
            first_pending
        };

        let local_indices: Vec<usize> = (resume_local..local_total)
            .filter(|&idx| idx < first_pending || pending_local.contains(&idx))
            .collect();

        match self.worker_sheet_map.get(from_printer) {
            Some(map) => local_indices
                .into_iter()
                .filter_map(|local_idx| map.get(local_idx).copied())
                .collect(),
            None => local_indices,
        }
    }

    /// Executes one-click failover: slices remaining imposed sheets directly via
    /// `pdfcraft_organize::extract_pages` (preserving encryption, form fields, rasterization,
    /// Poster/Booklet layouts, and duplex leaf alignment) and dispatches to `to_printer`.
    pub fn failover_reroute(&mut self, from_printer: &str, to_printer: &str) -> Result<(), FailoverError> {
        if self.master_cancel.is_cancelled() {
            return Err(FailoverError::BatchCancelled);
        }

        let (pending_local, local_total) = {
            let from_tel = self
                .telemetry
                .get(from_printer)
                .ok_or_else(|| FailoverError::PrinterNotFound(from_printer.to_string()))?;
            (from_tel.ledger.slice_for_failover(), from_tel.ledger.total_sheets)
        };
        if pending_local.is_empty() {
            return Err(FailoverError::NoPendingSheets(from_printer.to_string()));
        }

        if from_printer == to_printer {
            return Err(FailoverError::TargetPrinterBusy(to_printer.to_string()));
        }

        if let Some(target_tel) = self.telemetry.get(to_printer)
            && matches!(
                target_tel.health,
                StreamHealth::Healthy { .. }
                    | StreamHealth::Throttled { .. }
                    | StreamHealth::Stalled { .. }
            )
        {
            return Err(FailoverError::TargetPrinterBusy(to_printer.to_string()));
        }

        let global_slice = self.compute_failover_global_sheets(from_printer, &pending_local, local_total);
        if global_slice.is_empty() {
            return Err(FailoverError::NoPendingSheets(from_printer.to_string()));
        }
        let target_total_sheets = global_slice.len();

        self.cancel_printer(from_printer);
        if let Some(from_tel_mut) = self.telemetry.get_mut(from_printer)
            && !matches!(
                from_tel_mut.health,
                StreamHealth::Faulted { .. } | StreamHealth::Completed { .. }
            )
        {
            from_tel_mut.health = StreamHealth::Faulted {
                error: format!("Failover rerouted to '{to_printer}'"),
                recoverable: false,
            };
        }

        let mut target_job = self
            .printer_jobs
            .get(to_printer)
            .cloned()
            .unwrap_or_else(|| self.default_job.clone());
        target_job.printer = Some(to_printer.to_string());
        refresh_cups_orientation_for_slice(&mut target_job, &self.batch_sheets, &global_slice);

        let target_is_duplex = matches!(
            target_job.duplex,
            Some(pdfcraft_print::spool::Duplex::LongEdge | pdfcraft_print::spool::Duplex::ShortEdge)
        );
        let worker_cancel = CancellationToken::new();
        let spooling_active = Arc::new(AtomicBool::new(false));

        let task = WorkerTask {
            printer_name: to_printer.to_string(),
            batch_cache: self.batch_cache.clone(),
            slice_sheets: Some(global_slice.clone()),
            total_sheets: target_total_sheets,
            adapt_target_simplex: true,
            job: target_job,
            cancel_token: worker_cancel.clone(),
            master_cancel: self.master_cancel.clone(),
            spooling_active: spooling_active.clone(),
            notifier: self.notifier.clone(),
            custom_spooler: self.custom_spooler.clone(),
        };

        let initial_tel = PrinterStreamTelemetry {
            printer_name: to_printer.to_string(),
            current_sheet: 0,
            total_sheets: target_total_sheets,
            bytes_sent: 0,
            health: StreamHealth::Healthy {
                rate_kbps: 0.0,
                sheets_per_sec: 0.0,
            },
            last_progress_time: Instant::now(),
            ledger: CommittedSheetsLedger::new(target_total_sheets),
            error_message: None,
        };

        self.worker_sheet_map.insert(to_printer.to_string(), global_slice);
        self.worker_duplex.insert(to_printer.to_string(), target_is_duplex);
        self.telemetry.insert(to_printer.to_string(), initial_tel);
        self.workers.insert(
            to_printer.to_string(),
            WorkerControl {
                cancel_token: worker_cancel,
                spooling_active,
            },
        );

        let builder = std::thread::Builder::new().name(format!("print-worker-failover-{to_printer}"));
        if let Err(e) = builder.spawn(move || run_printer_worker(task)) {
            return Err(FailoverError::LayoutError(to_printer.to_string(), e.to_string()));
        }

        self.notifier.send(CoordinatorEvent::FailoverDispatched {
            from_printer: from_printer.to_string(),
            to_printer: to_printer.to_string(),
            rerouted_sheets: target_total_sheets,
        });

        Ok(())
    }
}

/// If `job.options` carries a CUPS `orientation-requested` entry from the full document, recompute
/// it strictly for the sliced failover sheets so mixed-orientation documents stay accurate.
fn refresh_cups_orientation_for_slice(
    job: &mut pdfcraft_print::spool::Job,
    batch_sheets: &[Sheet],
    slice_indices: &[usize],
) {
    if !job.options.iter().any(|(k, _)| k == "orientation-requested") {
        return;
    }
    job.options.retain(|(k, _)| k != "orientation-requested");
    let sliced: Vec<Sheet> = slice_indices
        .iter()
        .filter_map(|&idx| batch_sheets.get(idx).cloned())
        .collect();
    pdfcraft_print::add_cups_orientation(&mut job.options, &sliced);
}

impl Drop for MultiJobHandle {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// Multi-printer job coordinator.
pub struct PrintOrchestrationCoordinator;

impl PrintOrchestrationCoordinator {
    /// Dispatches a print job concurrently to multiple target printers using default spooler settings.
    /// Kept for backward compatibility; production UI workflows call [`Self::dispatch_batch`] or
    /// [`Self::dispatch_batch_prepared`].
    pub fn dispatch_multi(
        printers: &[String],
        settings: &Settings,
        doc_source: SharedPdfSource,
        page_sizes: &[(f64, f64)],
    ) -> Result<MultiJobHandle, PrintCoordinatorError> {
        Self::dispatch_batch(BatchDispatchConfig {
            printers: printers.to_vec(),
            settings: settings.clone(),
            doc_source,
            page_sizes: page_sizes.to_vec(),
            printer_jobs: HashMap::new(),
            default_job: pdfcraft_print::spool::Job::default(),
            pre_imposed_pdf: None,
            custom_spooler: None,
            on_event_wakeup: None,
        })
    }

    /// Dispatches a unified print batch concurrently across all selected printers, applying
    /// unified hardware ticket settings (`copies`, `collate`, `duplex`, `grayscale`, `paper`, `options`)
    /// and sharing zero-copy PDF buffers across isolated background worker threads.
    pub fn dispatch_batch(config: BatchDispatchConfig) -> Result<MultiJobHandle, PrintCoordinatorError> {
        Self::dispatch_batch_internal(config, None)
    }

    /// Dispatches a unified print batch using an O(1) [`PreparedPrintSource`] captured from
    /// [`crate::Session::prepare_print_pdf`], moving all imposition, rasterization, monochrome
    /// filtering, and GDI transparency flattening completely off the UI thread.
    pub fn dispatch_batch_prepared(
        config: BatchDispatchConfig,
        prepared: PreparedPrintSource,
    ) -> Result<MultiJobHandle, PrintCoordinatorError> {
        Self::dispatch_batch_internal(config, Some(prepared))
    }

    fn dispatch_batch_internal(
        config: BatchDispatchConfig,
        prepared_source: Option<PreparedPrintSource>,
    ) -> Result<MultiJobHandle, PrintCoordinatorError> {
        if config.printers.is_empty() {
            return Err(PrintCoordinatorError::NoPrintersSelected);
        }

        let batch_sheets = layout(&config.page_sizes, &config.settings)
            .map_err(|e| PrintCoordinatorError::Layout(e.to_string()))?;
        let total_sheets = batch_sheets.len();

        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        let governor = Arc::new(ConcurrencyGovernor::new(cpus.clamp(1, 4), cpus.clamp(2, 8)));

        let batch_cache = Arc::new(BatchPreparedCache::new(
            config.doc_source,
            prepared_source,
            config.pre_imposed_pdf,
            config.settings,
            governor,
        ));

        let (event_tx, event_rx) = channel();
        let notifier = EventNotifier {
            tx: event_tx,
            wakeup: config.on_event_wakeup,
        };
        let master_cancel = CancellationToken::new();
        let mut workers = HashMap::new();
        let mut telemetry = HashMap::new();
        let mut worker_sheet_map = HashMap::new();
        let mut worker_duplex = HashMap::new();
        let full_sheet_indices: Vec<usize> = (0..total_sheets).collect();

        for printer_name in &config.printers {
            let worker_cancel = CancellationToken::new();
            let spooling_active = Arc::new(AtomicBool::new(false));
            let mut job = config
                .printer_jobs
                .get(printer_name)
                .cloned()
                .unwrap_or_else(|| config.default_job.clone());
            job.printer = Some(printer_name.clone());
            let is_duplex = matches!(
                job.duplex,
                Some(pdfcraft_print::spool::Duplex::LongEdge | pdfcraft_print::spool::Duplex::ShortEdge)
            );

            let task = WorkerTask {
                printer_name: printer_name.clone(),
                batch_cache: batch_cache.clone(),
                slice_sheets: None,
                total_sheets,
                adapt_target_simplex: false,
                job,
                cancel_token: worker_cancel.clone(),
                master_cancel: master_cancel.clone(),
                spooling_active: spooling_active.clone(),
                notifier: notifier.clone(),
                custom_spooler: config.custom_spooler.clone(),
            };

            let initial_tel = PrinterStreamTelemetry {
                printer_name: printer_name.clone(),
                current_sheet: 0,
                total_sheets,
                bytes_sent: 0,
                health: StreamHealth::Healthy {
                    rate_kbps: 0.0,
                    sheets_per_sec: 0.0,
                },
                last_progress_time: Instant::now(),
                ledger: CommittedSheetsLedger::new(total_sheets),
                error_message: None,
            };
            telemetry.insert(printer_name.clone(), initial_tel);
            worker_sheet_map.insert(printer_name.clone(), full_sheet_indices.clone());
            worker_duplex.insert(printer_name.clone(), is_duplex);

            let builder = std::thread::Builder::new().name(format!("print-worker-{printer_name}"));
            if let Err(e) = builder.spawn(move || run_printer_worker(task)) {
                return Err(PrintCoordinatorError::ThreadSpawn(
                    printer_name.clone(),
                    e.to_string(),
                ));
            }

            workers.insert(
                printer_name.clone(),
                WorkerControl {
                    cancel_token: worker_cancel,
                    spooling_active,
                },
            );
        }

        notifier.send(CoordinatorEvent::BatchStarted {
            total_printers: config.printers.len(),
        });

        static BATCH_COUNTER: AtomicU64 = AtomicU64::new(1);
        let batch_id = BATCH_COUNTER.fetch_add(1, Ordering::Relaxed);

        Ok(MultiJobHandle {
            batch_id,
            workers,
            event_rx,
            notifier,
            telemetry,
            master_cancel,
            batch_cache,
            batch_sheets,
            worker_sheet_map,
            worker_duplex,
            printer_jobs: config.printer_jobs,
            default_job: config.default_job,
            custom_spooler: config.custom_spooler,
        })
    }
}

/// Self-contained execution bundle for a single isolated printer worker thread.
struct WorkerTask {
    printer_name: String,
    batch_cache: Arc<BatchPreparedCache>,
    slice_sheets: Option<Vec<usize>>,
    total_sheets: usize,
    adapt_target_simplex: bool,
    job: pdfcraft_print::spool::Job,
    cancel_token: CancellationToken,
    master_cancel: CancellationToken,
    spooling_active: Arc<AtomicBool>,
    notifier: EventNotifier,
    custom_spooler: Option<SpoolSubmitFn>,
}

impl WorkerTask {
    fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled() || self.master_cancel.is_cancelled()
    }

    fn emit_cancelled(&self) {
        self.spooling_active.store(false, Ordering::Release);
        self.notifier.send(CoordinatorEvent::PrinterCancelled {
            printer_name: self.printer_name.clone(),
        });
    }

    fn emit_failed(&self, mut ledger: CommittedSheetsLedger, error: String) {
        self.spooling_active.store(false, Ordering::Release);
        let failed_idx = ledger.committed_count();
        ledger.record_failure(failed_idx);
        self.notifier.send(CoordinatorEvent::PrinterFailed {
            printer_name: self.printer_name.clone(),
            error,
            committed_sheets: ledger.committed_sheets,
            pending_sheets: ledger.pending_sheets,
            suggested_failover: None,
        });
    }
}

/// Prepares the zero-copy spool-ready PDF buffer for `task` under CPU governance.
fn prepare_task_spool_bytes(task: &mut WorkerTask) -> Result<Arc<Vec<u8>>, String> {
    if task.adapt_target_simplex
        && task.custom_spooler.is_none()
        && matches!(
            task.job.duplex,
            Some(pdfcraft_print::spool::Duplex::LongEdge | pdfcraft_print::spool::Duplex::ShortEdge)
        )
        && pdfcraft_print::spool::two_sided(&task.printer_name) == Some(false)
    {
        task.job.duplex = None;
    }

    let shared_buffer = task.batch_cache.resolve(&task.cancel_token)?;
    let is_gdi =
        task.custom_spooler.is_none() && pdfcraft_print::spool::gdi_driver(&task.printer_name);

    if let Some(ref indices) = task.slice_sheets {
        let _cpu_permit = task
            .batch_cache
            .governor
            .acquire_cpu_permit(&task.cancel_token)
            .map_err(governor_err_string)?;
        return shared_buffer.slice_sheets(indices, &mut task.job, is_gdi);
    }

    let _cpu_permit = task
        .batch_cache
        .governor
        .acquire_cpu_permit(&task.cancel_token)
        .map_err(governor_err_string)?;
    Ok(shared_buffer.for_job(&mut task.job, is_gdi))
}

/// Background worker executing on an isolated thread.
fn run_printer_worker(mut task: WorkerTask) {
    let total_sheets = task.total_sheets;
    let mut ledger = CommittedSheetsLedger::new(total_sheets);
    let start_time = Instant::now();

    if task.is_cancelled() {
        task.emit_cancelled();
        return;
    }

    let spooled_bytes = match prepare_task_spool_bytes(&mut task) {
        Ok(b) => b,
        Err(error) => {
            if task.is_cancelled() {
                task.emit_cancelled();
            } else {
                task.emit_failed(ledger, error);
            }
            return;
        }
    };

    let _spool_permit = match task
        .batch_cache
        .governor
        .acquire_spool_permit(&task.cancel_token)
    {
        Ok(permit) => permit,
        Err(err) => {
            if task.is_cancelled() || matches!(err, GovernorError::Cancelled) {
                task.emit_cancelled();
            } else {
                task.emit_failed(ledger, governor_err_string(err));
            }
            return;
        }
    };

    if task.is_cancelled() {
        task.emit_cancelled();
        return;
    }

    // Mark active spooling and refresh last_progress_time right as transmission starts so
    // STALL_TIMEOUT applies strictly to active spooling rather than queue/prep wait time.
    let mut last_progress = Instant::now();
    task.spooling_active.store(true, Ordering::Release);
    task.notifier.send(CoordinatorEvent::ProgressUpdate(PrinterStreamTelemetry {
        printer_name: task.printer_name.clone(),
        current_sheet: 0,
        total_sheets,
        bytes_sent: 0,
        health: StreamHealth::Healthy {
            rate_kbps: 0.0,
            sheets_per_sec: 0.0,
        },
        last_progress_time: last_progress,
        ledger: ledger.clone(),
        error_message: None,
    }));

    let total_bytes = spooled_bytes.len() as u64;
    let bytes_per_sheet = total_bytes / (total_sheets.max(1) as u64);
    let printer_for_cb = task.printer_name.clone();
    let cancel_for_cb = task.cancel_token.clone();
    let master_for_cb = task.master_cancel.clone();
    let notifier_for_cb = task.notifier.clone();

    let mut on_progress = |sent_sheets: usize, total: usize| {
        if cancel_for_cb.is_cancelled() || master_for_cb.is_cancelled() {
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(last_progress).as_secs_f64().max(0.001);
        last_progress = now;
        ledger.mark_committed_up_to(sent_sheets);

        let sheets_per_sec = if dt > 0.0 { 1.0 / dt } else { 0.0 };
        let bytes_sent = (sent_sheets as u64) * bytes_per_sheet;
        let rate_kbps = (bytes_per_sheet as f64 / 1024.0) / dt;
        let health = if rate_kbps > 0.0 && rate_kbps < THROTTLE_THRESHOLD_KBPS {
            StreamHealth::Throttled {
                rate_kbps,
                reason: format!("Transfer rate ({rate_kbps:.1} KB/s) below threshold"),
            }
        } else {
            StreamHealth::Healthy {
                rate_kbps,
                sheets_per_sec,
            }
        };

        notifier_for_cb.send(CoordinatorEvent::ProgressUpdate(PrinterStreamTelemetry {
            printer_name: printer_for_cb.clone(),
            current_sheet: sent_sheets,
            total_sheets: total,
            bytes_sent,
            health,
            last_progress_time: now,
            ledger: ledger.clone(),
            error_message: None,
        }));
    };

    let submit_res = match &task.custom_spooler {
        Some(submit) => submit(&spooled_bytes, &task.job, &mut on_progress),
        None => pdfcraft_print::spool::submit_with_progress(&spooled_bytes, &task.job, &mut on_progress)
            .map_err(|e| e.to_string()),
    };
    task.spooling_active.store(false, Ordering::Release);

    if task.is_cancelled() {
        task.emit_cancelled();
        return;
    }

    match submit_res {
        Ok(_) => {
            ledger.mark_committed_up_to(total_sheets);
            task.notifier.send(CoordinatorEvent::PrinterCompleted {
                printer_name: task.printer_name,
                total_sheets,
                duration_secs: start_time.elapsed().as_secs_f64(),
            });
        }
        Err(error) => {
            task.emit_failed(ledger, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_committed_sheets_ledger_lifecycle() {
        let mut ledger = CommittedSheetsLedger::new(10);
        assert_eq!(ledger.total_sheets, 10);
        assert_eq!(ledger.committed_count(), 0);
        assert_eq!(ledger.pending_count(), 10);
        assert_eq!(ledger.pending_sheets, (0..10).collect::<Vec<_>>());
        assert!(!ledger.is_complete());

        // Commit sheets 0..4
        ledger.mark_committed_up_to(4);
        assert_eq!(ledger.committed_sheets, vec![0, 1, 2, 3]);
        assert_eq!(ledger.pending_sheets, vec![4, 5, 6, 7, 8, 9]);
        assert_eq!(ledger.committed_count(), 4);
        assert_eq!(ledger.pending_count(), 6);

        // Failover slice must match pending sheets exactly
        let slice = ledger.slice_for_failover();
        assert_eq!(slice, vec![4, 5, 6, 7, 8, 9]);

        // Record failure on sheet 4
        ledger.record_failure(4);
        assert_eq!(ledger.failed_sheet, Some(4));
        assert!(ledger.pending_sheets.contains(&4));

        // Mark individual commit
        ledger.mark_committed(4);
        assert_eq!(ledger.committed_count(), 5);
        assert!(!ledger.pending_sheets.contains(&4));

        // Complete remaining sheets
        ledger.mark_committed_up_to(10);
        assert!(ledger.is_complete());
        assert_eq!(ledger.pending_count(), 0);
    }

    #[test]
    fn test_stream_health_transitions() {
        let healthy = StreamHealth::Healthy { rate_kbps: 150.0, sheets_per_sec: 2.0 };
        assert!(matches!(healthy, StreamHealth::Healthy { .. }));

        let throttled = StreamHealth::Throttled { rate_kbps: 12.0, reason: "slow queue".into() };
        assert!(matches!(throttled, StreamHealth::Throttled { .. }));

        let stalled = StreamHealth::Stalled { stalled_duration_secs: 15 };
        assert!(matches!(stalled, StreamHealth::Stalled { stalled_duration_secs: 15 }));

        let faulted = StreamHealth::Faulted { error: "paper jam".into(), recoverable: true };
        assert!(matches!(faulted, StreamHealth::Faulted { recoverable: true, .. }));

        let completed = StreamHealth::Completed { total_sheets: 10, duration_secs: 4.2 };
        assert!(matches!(completed, StreamHealth::Completed { total_sheets: 10, .. }));
    }

    #[test]
    fn test_cancellation_token() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_failover_slicing_zero_duplicates() {
        let mut ledger = CommittedSheetsLedger::new(20);
        ledger.mark_committed_up_to(12); // sheets 0..11 committed
        let pending = ledger.slice_for_failover();
        assert_eq!(pending.len(), 8);
        assert_eq!(pending, (12..20).collect::<Vec<_>>());

        // Target ledger created from pending indices
        let target_ledger = CommittedSheetsLedger::from_pending_indices(8, &pending);
        assert_eq!(target_ledger.total_sheets, 8);
        assert_eq!(target_ledger.pending_sheets, vec![12, 13, 14, 15, 16, 17, 18, 19]);
        assert_eq!(target_ledger.committed_count(), 0);
    }

    #[test]
    fn test_coordinator_empty_printers_error() {
        let res = PrintOrchestrationCoordinator::dispatch_multi(
            &[],
            &Settings::default(),
            SharedPdfSource::from(vec![]),
            &[(612.0, 792.0)],
        );
        assert!(matches!(res, Err(PrintCoordinatorError::NoPrintersSelected)));
    }

    fn make_rgb_test_pdf(page_count: usize) -> Vec<u8> {
        use pdfcraft_cos::{Dict, Document, Object, SaveOptions, Stream, write_full};
        let mut doc = Document::new_empty();
        let pages_ref = doc
            .root()
            .and_then(|r| doc.get(r).as_dict().and_then(|d| d.reference(b"Pages")))
            .unwrap();
        let mut kids = Vec::new();
        for _ in 0..page_count {
            let stream = doc.add(Object::Stream(Stream::from_raw(
                Dict::new(),
                b"1 0 0 rg 50 50 200 200 re f".to_vec(),
            )));
            let mut page = Dict::new();
            page.set("Type", Object::name("Page"));
            page.set("Parent", Object::Ref(pages_ref));
            page.set("MediaBox", Object::Array([0, 0, 595, 842].map(Object::Int).to_vec()));
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
    fn test_dispatch_batch_propagates_unified_job_and_k_only_monochrome_to_all_printers() {
        use std::sync::Mutex;
        let pdf_bytes = make_rgb_test_pdf(2);
        let settings = Settings {
            pages: vec![0, 1],
            paper: (595.0, 842.0),
            ..Settings::default()
        };
        let cos = pdfcraft_cos::Document::open(Arc::new(pdf_bytes.clone())).unwrap();
        let imposed = pdfcraft_print::impose(&cos, &settings).unwrap();

        let captured: Arc<Mutex<HashMap<String, (pdfcraft_print::spool::Job, usize)>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let captured_cb = captured.clone();

        let spooler: SpoolSubmitFn = Arc::new(move |bytes, job, progress| {
            progress(1, 2);
            progress(2, 2);
            // Verify RGB operator was rewritten to DeviceGray
            let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
            for page in pdfcraft_model::pages(&doc) {
                if let Some(c_ref) = page.dict.reference(b"Contents")
                    && let pdfcraft_cos::Object::Stream(s) = &*doc.get(c_ref)
                {
                    let text = String::from_utf8_lossy(&s.decoded().unwrap()).into_owned();
                    assert!(!text.contains("1 0 0 rg"), "RGB fill must be converted to K-only: {text}");
                }
            }
            captured_cb
                .lock()
                .unwrap()
                .insert(job.printer.clone().unwrap(), (job.clone(), bytes.len()));
            Ok("queued".to_string())
        });

        let base_job = pdfcraft_print::spool::Job {
            printer: None,
            copies: 4,
            collate: true,
            duplex: Some(pdfcraft_print::spool::Duplex::LongEdge),
            scaling: pdfcraft_print::spool::Scaling::Exact,
            grayscale: true,
            grey_pages: false,
            title: "Invoice.pdf".to_string(),
            options: vec![("media".to_string(), "iso_a4_210x297mm".to_string())],
        };
        let mut simplex_job = base_job.clone();
        simplex_job.duplex = None;

        let mut printer_jobs = HashMap::new();
        printer_jobs.insert("HP_Laser".to_string(), base_job.clone());
        printer_jobs.insert("Canon_MFP".to_string(), base_job.clone());
        printer_jobs.insert("Brother_Simplex".to_string(), simplex_job);

        let wakeups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wakeups_cb = wakeups.clone();

        let mut handle = PrintOrchestrationCoordinator::dispatch_batch(BatchDispatchConfig {
            printers: vec![
                "HP_Laser".to_string(),
                "Canon_MFP".to_string(),
                "Brother_Simplex".to_string(),
            ],
            settings,
            doc_source: SharedPdfSource::from(pdf_bytes),
            page_sizes: vec![(595.0, 842.0); 2],
            printer_jobs,
            default_job: base_job,
            pre_imposed_pdf: Some(Arc::new(imposed)),
            custom_spooler: Some(spooler),
            on_event_wakeup: Some(Arc::new(move || {
                wakeups_cb.fetch_add(1, Ordering::Relaxed);
            })),
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_all_completed() && Instant::now() < deadline {
            let _ = handle.poll_events();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_all_completed());
        assert!(
            wakeups.load(Ordering::Relaxed) >= 4,
            "on_event_wakeup must fire on batch start, progress, and completion"
        );

        let records = captured.lock().unwrap();
        assert_eq!(records.len(), 3);
        for name in ["HP_Laser", "Canon_MFP", "Brother_Simplex"] {
            let (job, len) = records.get(name).expect(name);
            assert!(len > &0);
            assert_eq!(job.copies, 4);
            assert!(job.collate);
            assert!(job.grayscale);
            assert!(job.grey_pages);
            assert_eq!(job.title, "Invoice.pdf");
            assert_eq!(
                job.options,
                vec![("media".to_string(), "iso_a4_210x297mm".to_string())]
            );
        }
        assert_eq!(
            records["HP_Laser"].0.duplex,
            Some(pdfcraft_print::spool::Duplex::LongEdge)
        );
        assert_eq!(records["Brother_Simplex"].0.duplex, None);
    }

    #[test]
    fn test_failover_reroute_preserves_unified_job_settings_and_slices_pending_sheets() {
        use std::sync::Mutex;
        let pdf_bytes = make_rgb_test_pdf(4);
        let settings = Settings {
            pages: vec![0, 1, 2, 3],
            paper: (595.0, 842.0),
            ..Settings::default()
        };

        let captured: Arc<Mutex<HashMap<String, (pdfcraft_print::spool::Job, usize)>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let captured_cb = captured.clone();

        let spooler: SpoolSubmitFn = Arc::new(move |bytes, job, progress| {
            let printer = job.printer.clone().unwrap_or_default();
            if printer == "Printer_A" {
                progress(2, 4);
                return Err("Paper Jam at Tray 2".to_string());
            }
            let doc = pdfcraft_cos::Document::open(Arc::new(bytes.to_vec())).unwrap();
            let rerouted_page_count = pdfcraft_model::pages(&doc).len();
            progress(rerouted_page_count, rerouted_page_count);
            captured_cb
                .lock()
                .unwrap()
                .insert(printer, (job.clone(), rerouted_page_count));
            Ok("completed".to_string())
        });

        let default_job = pdfcraft_print::spool::Job {
            printer: None,
            copies: 3,
            collate: true,
            duplex: Some(pdfcraft_print::spool::Duplex::ShortEdge),
            scaling: pdfcraft_print::spool::Scaling::Exact,
            grayscale: true,
            grey_pages: false,
            title: "Contract.pdf".to_string(),
            options: vec![("media".to_string(), "iso_a4_210x297mm".to_string())],
        };

        let mut handle = PrintOrchestrationCoordinator::dispatch_batch(BatchDispatchConfig {
            printers: vec!["Printer_A".to_string()],
            settings,
            doc_source: SharedPdfSource::from(pdf_bytes),
            page_sizes: vec![(595.0, 842.0); 4],
            printer_jobs: HashMap::new(),
            default_job,
            pre_imposed_pdf: None,
            custom_spooler: Some(spooler),
            on_event_wakeup: None,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_all_completed() && Instant::now() < deadline {
            let _ = handle.poll_events();
            std::thread::sleep(Duration::from_millis(10));
        }

        let tel_a = handle.get_telemetry("Printer_A").unwrap();
        assert_eq!(tel_a.ledger.committed_sheets, vec![0, 1]);
        assert_eq!(tel_a.ledger.pending_sheets, vec![2, 3]);

        handle.failover_reroute("Printer_A", "Printer_B").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_all_completed() && Instant::now() < deadline {
            let _ = handle.poll_events();
            std::thread::sleep(Duration::from_millis(10));
        }

        let records = captured.lock().unwrap();
        let (job_b, page_count_b) = records.get("Printer_B").expect("Printer_B must receive reroute");
        assert_eq!(*page_count_b, 2, "Only the 2 uncommitted sheets must be rerouted");
        assert_eq!(job_b.copies, 3);
        assert!(job_b.grayscale);
        assert!(job_b.grey_pages);
        assert_eq!(job_b.duplex, Some(pdfcraft_print::spool::Duplex::ShortEdge));
        assert_eq!(job_b.title, "Contract.pdf");
    }
}
