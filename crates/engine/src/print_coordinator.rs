//! Multi-printer print orchestration coordinator with isolated fault boundaries,
//! per-printer ledger tracking, stream health monitoring, and one-click failover rerouting.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use pdfcraft_print::{Settings, layout, sheet_pages};

/// Stall detection timeout: if a stream receives no progress updates for this duration,
/// it transitions to `StreamHealth::Stalled`.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Low-bandwidth threshold in KB/s below which an active stream is marked as `StreamHealth::Throttled`.
pub const THROTTLE_THRESHOLD_KBPS: f64 = 25.0;

/// Shared read-only PDF document bytes backing concurrent print workers.
pub use crate::snapshot::SharedPdfSource;

/// Cooperative cancellation token shared across supervisor, workers, and UI.
#[derive(Clone, Default, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
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
    /// No progress updates received within `STALL_TIMEOUT`.
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

/// Shared lazy K-only monochrome conversion cache across concurrent workers in a batch.
type SharedMonoCache = Arc<std::sync::OnceLock<Option<Arc<Vec<u8>>>>>;

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
    /// Optional custom spooler hook (defaults to OS `pdfcraft_print::spool::submit_with_progress`).
    pub custom_spooler: Option<SpoolSubmitFn>,
}

/// Control structure for an individual worker.
struct WorkerControl {
    cancel_token: CancellationToken,
}

/// Control handle for an active multi-printer print batch.
pub struct MultiJobHandle {
    pub batch_id: u64,
    workers: HashMap<String, WorkerControl>,
    event_rx: Receiver<CoordinatorEvent>,
    event_tx: Sender<CoordinatorEvent>,
    telemetry: HashMap<String, PrinterStreamTelemetry>,
    master_cancel: CancellationToken,
    doc_source: SharedPdfSource,
    original_settings: Settings,
    page_sizes: Vec<(f64, f64)>,
    sheet_to_pages: Vec<Vec<usize>>,
    printer_jobs: HashMap<String, pdfcraft_print::spool::Job>,
    default_job: pdfcraft_print::spool::Job,
    custom_spooler: Option<SpoolSubmitFn>,
}

impl MultiJobHandle {
    /// Non-blocking event poll called on the UI thread (<0.2ms per frame).
    /// Drains available events, updates internal telemetry, and detects stalled streams.
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
            if matches!(tel.health, StreamHealth::Healthy { .. } | StreamHealth::Throttled { .. }) {
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
            CoordinatorEvent::PrinterCompleted { printer_name, total_sheets, duration_secs } => {
                if let Some(tel) = self.telemetry.get_mut(printer_name) {
                    tel.health = StreamHealth::Completed {
                        total_sheets: *total_sheets,
                        duration_secs: *duration_secs,
                    };
                    tel.ledger.mark_committed_up_to(*total_sheets);
                }
            }
            CoordinatorEvent::PrinterFailed { printer_name, error, committed_sheets, pending_sheets, .. } => {
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
        }
    }

    /// Returns true if all printers in the batch have reached a terminal state (Completed, Faulted).
    pub fn is_all_completed(&self) -> bool {
        !self.telemetry.is_empty()
            && self.telemetry.values().all(|t| {
                matches!(t.health, StreamHealth::Completed { .. } | StreamHealth::Faulted { .. })
            })
    }

    /// Executes one-click failover: slices pending sheets from `from_printer`, adapts margins,
    /// and dispatches to `to_printer` with zero duplicate pages.
    pub fn failover_reroute(&mut self, from_printer: &str, to_printer: &str) -> Result<(), FailoverError> {
        if self.master_cancel.is_cancelled() {
            return Err(FailoverError::BatchCancelled);
        }

        let from_tel = self
            .telemetry
            .get(from_printer)
            .ok_or_else(|| FailoverError::PrinterNotFound(from_printer.to_string()))?;

        let pending_sheets = from_tel.ledger.slice_for_failover();
        if pending_sheets.is_empty() {
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

        let mut pending_pages = Vec::new();
        for &sheet_idx in &pending_sheets {
            if let Some(pages) = self.sheet_to_pages.get(sheet_idx) {
                for &p in pages {
                    if !pending_pages.contains(&p) {
                        pending_pages.push(p);
                    }
                }
            }
        }

        let mut target_settings = self.original_settings.clone();
        target_settings.pages = pending_pages;

        let target_sheets = layout(&self.page_sizes, &target_settings)
            .map_err(|e| FailoverError::LayoutError(to_printer.to_string(), e.to_string()))?;
        let target_sheet_to_pages = sheet_pages(&target_sheets);
        let target_total_sheets = target_sheets.len();

        // Halt the stalled/failed printer thread and prevent duplicate page printing
        self.cancel_printer(from_printer);
        if let Some(from_tel_mut) = self.telemetry.get_mut(from_printer)
            && !matches!(from_tel_mut.health, StreamHealth::Faulted { .. } | StreamHealth::Completed { .. })
        {
            from_tel_mut.health = StreamHealth::Faulted {
                error: format!("Failover rerouted to '{to_printer}'"),
                recoverable: false,
            };
        }

        let worker_cancel = CancellationToken::new();
        let mut target_job = self
            .printer_jobs
            .get(to_printer)
            .cloned()
            .unwrap_or_else(|| self.default_job.clone());
        target_job.printer = Some(to_printer.to_string());
        if matches!(
            target_job.duplex,
            Some(pdfcraft_print::spool::Duplex::LongEdge | pdfcraft_print::spool::Duplex::ShortEdge)
        ) && self.custom_spooler.is_none()
            && pdfcraft_print::spool::two_sided(to_printer) == Some(false)
        {
            target_job.duplex = None;
        }

        let task = WorkerTask {
            printer_name: to_printer.to_string(),
            settings: target_settings,
            doc_source: self.doc_source.clone(),
            pre_imposed_pdf: None,
            mono_cache: Arc::new(std::sync::OnceLock::new()),
            job: target_job,
            sheet_to_pages: target_sheet_to_pages,
            cancel_token: worker_cancel.clone(),
            master_cancel: self.master_cancel.clone(),
            event_tx: self.event_tx.clone(),
            custom_spooler: self.custom_spooler.clone(),
        };

        let initial_tel = PrinterStreamTelemetry {
            printer_name: to_printer.to_string(),
            current_sheet: 0,
            total_sheets: target_total_sheets,
            bytes_sent: 0,
            health: StreamHealth::Healthy { rate_kbps: 0.0, sheets_per_sec: 0.0 },
            last_progress_time: Instant::now(),
            ledger: CommittedSheetsLedger::new(target_total_sheets),
            error_message: None,
        };

        self.telemetry.insert(to_printer.to_string(), initial_tel);
        self.workers.insert(to_printer.to_string(), WorkerControl { cancel_token: worker_cancel });

        let builder = std::thread::Builder::new().name(format!("print-worker-failover-{}", to_printer));
        let _ = builder.spawn(move || {
            run_printer_worker(task);
        });

        let _ = self.event_tx.send(CoordinatorEvent::FailoverDispatched {
            from_printer: from_printer.to_string(),
            to_printer: to_printer.to_string(),
            rerouted_sheets: target_total_sheets,
        });

        Ok(())
    }
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
    /// Kept for backward compatibility; production UI workflows call [`Self::dispatch_batch`].
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
        })
    }

    /// Dispatches a unified print batch concurrently across all selected printers, applying
    /// unified hardware ticket settings (`copies`, `collate`, `duplex`, `grayscale`, `paper`, `options`)
    /// and sharing zero-copy PDF buffers across isolated background worker threads.
    pub fn dispatch_batch(config: BatchDispatchConfig) -> Result<MultiJobHandle, PrintCoordinatorError> {
        if config.printers.is_empty() {
            return Err(PrintCoordinatorError::NoPrintersSelected);
        }

        let sheets = layout(&config.page_sizes, &config.settings)
            .map_err(|e| PrintCoordinatorError::Layout(e.to_string()))?;
        let sheet_to_pages = sheet_pages(&sheets);
        let total_sheets = sheets.len();

        let (event_tx, event_rx) = channel();
        let master_cancel = CancellationToken::new();
        let mut workers = HashMap::new();
        let mut telemetry = HashMap::new();
        let mono_cache: SharedMonoCache = Arc::new(std::sync::OnceLock::new());

        for printer_name in &config.printers {
            let worker_cancel = CancellationToken::new();
            let mut job = config
                .printer_jobs
                .get(printer_name)
                .cloned()
                .unwrap_or_else(|| config.default_job.clone());
            job.printer = Some(printer_name.clone());

            let task = WorkerTask {
                printer_name: printer_name.clone(),
                settings: config.settings.clone(),
                doc_source: config.doc_source.clone(),
                pre_imposed_pdf: config.pre_imposed_pdf.clone(),
                mono_cache: mono_cache.clone(),
                job,
                sheet_to_pages: sheet_to_pages.clone(),
                cancel_token: worker_cancel.clone(),
                master_cancel: master_cancel.clone(),
                event_tx: event_tx.clone(),
                custom_spooler: config.custom_spooler.clone(),
            };

            let initial_tel = PrinterStreamTelemetry {
                printer_name: printer_name.clone(),
                current_sheet: 0,
                total_sheets,
                bytes_sent: 0,
                health: StreamHealth::Healthy { rate_kbps: 0.0, sheets_per_sec: 0.0 },
                last_progress_time: Instant::now(),
                ledger: CommittedSheetsLedger::new(total_sheets),
                error_message: None,
            };
            telemetry.insert(printer_name.clone(), initial_tel);

            let builder = std::thread::Builder::new().name(format!("print-worker-{}", printer_name));
            let spawned = builder.spawn(move || {
                run_printer_worker(task);
            });

            if let Err(e) = spawned {
                return Err(PrintCoordinatorError::ThreadSpawn(printer_name.clone(), e.to_string()));
            }

            workers.insert(printer_name.clone(), WorkerControl { cancel_token: worker_cancel });
        }

        let _ = event_tx.send(CoordinatorEvent::BatchStarted {
            total_printers: config.printers.len(),
        });

        static BATCH_COUNTER: AtomicU64 = AtomicU64::new(1);
        let batch_id = BATCH_COUNTER.fetch_add(1, Ordering::Relaxed);

        Ok(MultiJobHandle {
            batch_id,
            workers,
            event_rx,
            event_tx,
            telemetry,
            master_cancel,
            doc_source: config.doc_source,
            original_settings: config.settings,
            page_sizes: config.page_sizes,
            sheet_to_pages,
            printer_jobs: config.printer_jobs,
            default_job: config.default_job,
            custom_spooler: config.custom_spooler,
        })
    }
}

/// Self-contained execution bundle for a single isolated printer worker thread.
struct WorkerTask {
    printer_name: String,
    settings: Settings,
    doc_source: SharedPdfSource,
    pre_imposed_pdf: Option<Arc<Vec<u8>>>,
    mono_cache: SharedMonoCache,
    job: pdfcraft_print::spool::Job,
    sheet_to_pages: Vec<Vec<usize>>,
    cancel_token: CancellationToken,
    master_cancel: CancellationToken,
    event_tx: Sender<CoordinatorEvent>,
    custom_spooler: Option<SpoolSubmitFn>,
}

/// Prepares print-ready PDF bytes for one worker, applying Dual-Layer K-Only Monochrome filtering
/// when `job.grayscale` is enabled while sharing cached output across sibling workers.
fn prepare_worker_pdf(
    doc_source: &SharedPdfSource,
    pre_imposed_pdf: Option<&Arc<Vec<u8>>>,
    mono_cache: &SharedMonoCache,
    settings: &Settings,
    job: &mut pdfcraft_print::spool::Job,
) -> Result<Vec<u8>, String> {
    if let Some(pdf_arc) = pre_imposed_pdf {
        if job.grayscale {
            let cached = mono_cache.get_or_init(|| {
                pdfcraft_print::monochrome::MonochromeVectorFilter::filter_document_to_k_only(pdf_arc)
                    .ok()
                    .map(Arc::new)
            });
            if let Some(mono_arc) = cached {
                job.grey_pages = true;
                return Ok(mono_arc.as_ref().clone());
            }
        }
        return Ok(pdf_arc.as_ref().clone());
    }

    let doc_arc = match doc_source {
        #[cfg(not(target_arch = "wasm32"))]
        SharedPdfSource::Mmap(m) => Arc::new(m.as_ref().to_vec()),
        SharedPdfSource::Heap(h) => h.clone(),
    };
    let cos_doc = pdfcraft_cos::Document::open(doc_arc)
        .map_err(|e| format!("Failed to open PDF document: {e}"))?;
    let imposed = pdfcraft_print::impose(&cos_doc, settings)
        .map_err(|e| format!("Imposition failure: {e}"))?;

    if job.grayscale
        && let Ok(mono_bytes) =
            pdfcraft_print::monochrome::MonochromeVectorFilter::filter_document_to_k_only(&imposed)
    {
        job.grey_pages = true;
        return Ok(mono_bytes);
    }

    Ok(imposed)
}

/// Background worker executing on an isolated thread.
fn run_printer_worker(mut task: WorkerTask) {
    let total_sheets = task.sheet_to_pages.len();
    let mut ledger = CommittedSheetsLedger::new(total_sheets);
    let start_time = Instant::now();
    let mut last_progress = Instant::now();

    if task.cancel_token.is_cancelled() || task.master_cancel.is_cancelled() {
        let _ = task.event_tx.send(CoordinatorEvent::PrinterCancelled {
            printer_name: task.printer_name,
        });
        return;
    }

    let imposed_bytes = match prepare_worker_pdf(
        &task.doc_source,
        task.pre_imposed_pdf.as_ref(),
        &task.mono_cache,
        &task.settings,
        &mut task.job,
    ) {
        Ok(b) => b,
        Err(error) => {
            ledger.record_failure(0);
            let _ = task.event_tx.send(CoordinatorEvent::PrinterFailed {
                printer_name: task.printer_name,
                error,
                committed_sheets: ledger.committed_sheets,
                pending_sheets: ledger.pending_sheets,
                suggested_failover: None,
            });
            return;
        }
    };

    let is_gdi = task.custom_spooler.is_none()
        && pdfcraft_print::spool::gdi_driver(&task.printer_name);
    let spooled_bytes = crate::spooler::spooler_copy(imposed_bytes, &mut task.job, is_gdi);

    if task.cancel_token.is_cancelled() || task.master_cancel.is_cancelled() {
        let _ = task.event_tx.send(CoordinatorEvent::PrinterCancelled {
            printer_name: task.printer_name,
        });
        return;
    }

    let total_bytes = spooled_bytes.len() as u64;
    let bytes_per_sheet = total_bytes / (total_sheets.max(1) as u64);
    let printer_for_cb = task.printer_name.clone();
    let cancel_for_cb = task.cancel_token.clone();
    let master_for_cb = task.master_cancel.clone();
    let tx_for_cb = task.event_tx.clone();

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
                reason: format!("Transfer rate ({:.1} KB/s) below threshold", rate_kbps),
            }
        } else {
            StreamHealth::Healthy {
                rate_kbps,
                sheets_per_sec,
            }
        };

        let telemetry = PrinterStreamTelemetry {
            printer_name: printer_for_cb.clone(),
            current_sheet: sent_sheets,
            total_sheets: total,
            bytes_sent,
            health,
            last_progress_time: now,
            ledger: ledger.clone(),
            error_message: None,
        };

        let _ = tx_for_cb.send(CoordinatorEvent::ProgressUpdate(telemetry));
    };

    let submit_res = match &task.custom_spooler {
        Some(submit) => submit(&spooled_bytes, &task.job, &mut on_progress),
        None => pdfcraft_print::spool::submit_with_progress(&spooled_bytes, &task.job, &mut on_progress)
            .map_err(|e| e.to_string()),
    };

    let duration_secs = start_time.elapsed().as_secs_f64();

    match submit_res {
        Ok(_) => {
            ledger.mark_committed_up_to(total_sheets);
            let _ = task.event_tx.send(CoordinatorEvent::PrinterCompleted {
                printer_name: task.printer_name,
                total_sheets,
                duration_secs,
            });
        }
        Err(error) => {
            let failed_idx = ledger.committed_count();
            ledger.record_failure(failed_idx);
            let _ = task.event_tx.send(CoordinatorEvent::PrinterFailed {
                printer_name: task.printer_name,
                error,
                committed_sheets: ledger.committed_sheets,
                pending_sheets: ledger.pending_sheets,
                suggested_failover: None,
            });
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
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_all_completed() && Instant::now() < deadline {
            let _ = handle.poll_events();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_all_completed());

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
