use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fault trigger condition.
#[derive(Debug, Clone, PartialEq)]
pub enum FaultTrigger {
    Immediate,
    AtSheet(usize),
    AfterBytes(u64),
    AfterElapsed(Duration),
}

/// The specific failure mode or impairment to inject.
#[derive(Debug, Clone, PartialEq)]
pub enum FaultAction {
    PaperJam { reason: String },
    DeviceDisconnected { error_code: u32, message: String },
    Throttle { rate_kbps: f64 },
    StallTimeout { duration: Duration },
    SpoolDiskFull,
    CorruptPayload { flip_byte_offset: usize },
}

/// Programmable rule bound to a specific printer or all printers.
#[derive(Debug, Clone)]
pub struct FaultRule {
    pub rule_id: &'static str,
    pub target_printer: Option<String>, // None matches all printers
    pub trigger: FaultTrigger,
    pub action: FaultAction,
    pub armed: bool,
}

/// Event recorded in fault execution audit log.
#[derive(Debug, Clone)]
pub struct FaultAuditRecord {
    pub timestamp: Instant,
    pub rule_id: &'static str,
    pub printer_name: String,
    pub sheet_index: usize,
    pub action_injected: FaultAction,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum InjectedFaultError {
    #[error("Hardware Paper Jam at sheet {sheet}: {reason}")]
    PaperJam { sheet: usize, reason: String },

    #[error("Device disconnected (code 0x{error_code:X}): {message}")]
    DeviceDisconnected { error_code: u32, message: String },

    #[error("Spool disk full: out of available storage")]
    SpoolDiskFull,

    #[error("Stream stalled and timed out after {0:?}")]
    StallTimeout(Duration),
}

/// Central programmable fault injector controller.
#[derive(Clone, Default)]
pub struct FaultInjector {
    rules: Arc<Mutex<Vec<FaultRule>>>,
    audit_log: Arc<Mutex<Vec<FaultAuditRecord>>>,
    start_time: Arc<Mutex<Option<Instant>>>,
}

impl FaultInjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_batch(&self) {
        let mut start = self.start_time.lock().unwrap();
        *start = Some(Instant::now());
    }

    pub fn add_rule(&self, rule: FaultRule) {
        self.rules.lock().unwrap().push(rule);
    }

    /// Hook executed before spooling a sheet.
    pub fn intercept_sheet_start(
        &self,
        printer_name: &str,
        sheet_index: usize,
        sheet_bytes: usize,
    ) -> Result<(), InjectedFaultError> {
        let mut rules = self.rules.lock().unwrap();
        let elapsed = self
            .start_time
            .lock()
            .unwrap()
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO);

        for rule in rules.iter_mut() {
            if !rule.armed {
                continue;
            }
            if let Some(ref target) = rule.target_printer
                && target != printer_name
            {
                continue;
            }

            let triggered = match rule.trigger {
                FaultTrigger::Immediate => true,
                FaultTrigger::AtSheet(target_sheet) => sheet_index == target_sheet,
                FaultTrigger::AfterElapsed(target_dur) => elapsed >= target_dur,
                _ => false,
            };

            if triggered {
                rule.armed = false; // Disarm one-shot rule
                self.record_audit(rule.rule_id, printer_name, sheet_index, &rule.action);

                let action = rule.action.clone();
                drop(rules); // Drop mutex guard BEFORE sleeping to prevent blocking other printer threads

                return match &action {
                    FaultAction::PaperJam { reason } => Err(InjectedFaultError::PaperJam {
                        sheet: sheet_index,
                        reason: reason.clone(),
                    }),
                    FaultAction::DeviceDisconnected { error_code, message } => {
                        Err(InjectedFaultError::DeviceDisconnected {
                            error_code: *error_code,
                            message: message.clone(),
                        })
                    }
                    FaultAction::SpoolDiskFull => Err(InjectedFaultError::SpoolDiskFull),
                    FaultAction::StallTimeout { duration } => {
                        std::thread::sleep(*duration);
                        Err(InjectedFaultError::StallTimeout(*duration))
                    }
                    FaultAction::Throttle { rate_kbps } => {
                        let delay_secs = (sheet_bytes as f64) / (rate_kbps * 128.0);
                        std::thread::sleep(Duration::from_secs_f64(delay_secs));
                        Ok(())
                    }
                    _ => Ok(()),
                };
            }
        }
        Ok(())
    }

    fn record_audit(
        &self,
        rule_id: &'static str,
        printer: &str,
        sheet: usize,
        action: &FaultAction,
    ) {
        self.audit_log.lock().unwrap().push(FaultAuditRecord {
            timestamp: Instant::now(),
            rule_id,
            printer_name: printer.to_string(),
            sheet_index: sheet,
            action_injected: action.clone(),
        });
    }

    pub fn audit_log(&self) -> Vec<FaultAuditRecord> {
        self.audit_log.lock().unwrap().clone()
    }
}

pub struct FaultAssertions;

impl FaultAssertions {
    /// Verifies that when Printer A failed at sheet k, failover completed sheets k..N,
    /// with zero duplicate sheets and zero omitted sheets.
    pub fn assert_failover_exact_sheets(
        failed_printer_sheets: &[usize],
        failover_printer_sheets: &[usize],
        expected_total_sheets: usize,
        jam_sheet: usize,
    ) {
        // 1. Failed printer should have committed exactly 0..jam_sheet
        let expected_failed_commits: Vec<usize> = (0..jam_sheet).collect();
        assert_eq!(
            failed_printer_sheets,
            expected_failed_commits.as_slice(),
            "Failed printer committed incorrect sheets"
        );

        // 2. Failover printer should have committed exactly jam_sheet..expected_total_sheets
        let expected_failover_commits: Vec<usize> = (jam_sheet..expected_total_sheets).collect();
        assert_eq!(
            failover_printer_sheets,
            expected_failover_commits.as_slice(),
            "Failover printer received duplicate or dropped sheets"
        );

        // 3. Total unique sheets must match exactly
        assert_eq!(
            failed_printer_sheets.len() + failover_printer_sheets.len(),
            expected_total_sheets,
            "Total combined committed sheets does not equal expected sheet count"
        );
    }
}
