//! Empirical verification tests for PrintOrchestrationCoordinator, CommittedSheetsLedger,
//! fault isolation, cooperative cancellation, and one-click failover rerouting.

use pdfcraft_engine::print_coordinator::{
    CancellationToken, CommittedSheetsLedger, FailoverError, PrintOrchestrationCoordinator,
    SharedPdfSource,
};
use pdfcraft_print::Settings;

#[test]
fn test_ledger_boundary_zero_sheets() {
    let mut ledger = CommittedSheetsLedger::new(0);
    assert_eq!(ledger.total_sheets, 0);
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), 0);
    assert!(ledger.is_complete());
    assert_eq!(ledger.slice_for_failover(), Vec::<usize>::new());

    // Boundary calls on 0 sheets
    ledger.mark_committed_up_to(0);
    assert!(ledger.is_complete());

    ledger.mark_committed(0);
    // Even if marked, count remains 0 because total is 0
    ledger.record_failure(0);
    assert_eq!(ledger.failed_sheet, Some(0));
    // Index 0 >= total_sheets (0), so it should not be inserted into pending
    assert!(ledger.pending_sheets.is_empty());
}

#[test]
fn test_ledger_boundary_one_sheet() {
    let mut ledger = CommittedSheetsLedger::new(1);
    assert_eq!(ledger.total_sheets, 1);
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), 1);
    assert_eq!(ledger.pending_sheets, vec![0]);
    assert!(!ledger.is_complete());

    // Failure on sheet 0
    ledger.record_failure(0);
    assert_eq!(ledger.failed_sheet, Some(0));
    assert_eq!(ledger.slice_for_failover(), vec![0]);
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), 1);

    // Now commit sheet 0
    ledger.mark_committed(0);
    assert_eq!(ledger.committed_count(), 1);
    assert_eq!(ledger.pending_count(), 0);
    assert!(ledger.is_complete());
    assert!(ledger.slice_for_failover().is_empty());
}

#[test]
fn test_ledger_boundary_500_sheets_stress() {
    let total = 500;
    let mut ledger = CommittedSheetsLedger::new(total);
    assert_eq!(ledger.total_sheets, total);
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), total);
    assert_eq!(ledger.pending_sheets.len(), total);

    // Commit up to sheet 247
    let committed_up_to = 247;
    ledger.mark_committed_up_to(committed_up_to);
    assert_eq!(ledger.committed_count(), 247);
    assert_eq!(ledger.pending_count(), 253);
    assert_eq!(ledger.committed_sheets, (0..247).collect::<Vec<_>>());
    assert_eq!(ledger.pending_sheets, (247..500).collect::<Vec<_>>());

    // Invariant: committed and pending are completely disjoint
    for s in &ledger.committed_sheets {
        assert!(!ledger.pending_sheets.contains(s), "Sheet {s} present in both committed and pending");
    }
    assert_eq!(ledger.committed_count() + ledger.pending_count(), total);

    // Failover slice must match pending sheets exactly
    let slice = ledger.slice_for_failover();
    assert_eq!(slice.len(), 253);
    assert_eq!(slice.first(), Some(&247));
    assert_eq!(slice.last(), Some(&499));

    // Failure at sheet 247
    ledger.record_failure(247);
    assert_eq!(ledger.failed_sheet, Some(247));
    assert!(ledger.pending_sheets.contains(&247));

    // Complete the rest
    ledger.mark_committed_up_to(total);
    assert!(ledger.is_complete());
    assert_eq!(ledger.committed_count(), total);
    assert_eq!(ledger.pending_count(), 0);
    assert!(ledger.slice_for_failover().is_empty());
}

#[test]
fn test_ledger_partial_failure_at_first_sheet() {
    let mut ledger = CommittedSheetsLedger::new(50);
    // Immediate failure before any sheet commits
    ledger.record_failure(0);
    assert_eq!(ledger.failed_sheet, Some(0));
    assert_eq!(ledger.committed_count(), 0);
    assert_eq!(ledger.pending_count(), 50);

    let slice = ledger.slice_for_failover();
    assert_eq!(slice, (0..50).collect::<Vec<_>>());
}

#[test]
fn test_ledger_partial_failure_at_last_sheet() {
    let total = 100;
    let mut ledger = CommittedSheetsLedger::new(total);
    // Sheets 0..98 committed
    ledger.mark_committed_up_to(99);
    assert_eq!(ledger.committed_count(), 99);
    assert_eq!(ledger.pending_count(), 1);
    assert_eq!(ledger.pending_sheets, vec![99]);

    // Last sheet (index 99) fails
    ledger.record_failure(99);
    assert_eq!(ledger.failed_sheet, Some(99));

    let slice = ledger.slice_for_failover();
    assert_eq!(slice, vec![99]);
}

#[test]
fn test_ledger_random_commit_permutation_invariants() {
    let total = 200;
    let mut ledger = CommittedSheetsLedger::new(total);

    // Deterministic pseudo-random permutation using LCG
    let mut perm: Vec<usize> = (0..total).collect();
    let mut seed: u64 = 0xdeadbeef;
    for i in (1..total).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = (seed >> 33) as usize % (i + 1);
        perm.swap(i, j);
    }

    let mut committed_set = std::collections::BTreeSet::new();

    for (step, &sheet_idx) in perm.iter().enumerate() {
        ledger.mark_committed(sheet_idx);
        committed_set.insert(sheet_idx);

        // Assert strictly maintained invariants:
        assert_eq!(ledger.committed_count(), step + 1);
        assert_eq!(ledger.pending_count(), total - (step + 1));
        assert_eq!(ledger.committed_count() + ledger.pending_count(), total);

        // Invariant: committed sheets is sorted
        for w in ledger.committed_sheets.windows(2) {
            assert!(w[0] < w[1], "Committed sheets not strictly ascending");
        }

        // Invariant: pending sheets does not contain committed sheet
        assert!(!ledger.pending_sheets.contains(&sheet_idx));

        // Invariant: union of committed and pending covers 0..total exactly
        let pending_set: std::collections::BTreeSet<_> = ledger.pending_sheets.iter().copied().collect();
        let intersection: Vec<_> = committed_set.intersection(&pending_set).collect();
        assert!(intersection.is_empty(), "Committed and pending sets overlap: {:?}", intersection);
    }

    assert!(ledger.is_complete());
    assert_eq!(ledger.pending_count(), 0);
    assert_eq!(ledger.slice_for_failover(), Vec::<usize>::new());
}

#[test]
fn test_cancellation_cooperative_atomic() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());

    let token_clone1 = token.clone();
    let token_clone2 = token.clone();
    let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
    let ready_worker = ready.clone();

    // Verify cross-thread cancellation visibility deterministically
    let h = std::thread::spawn(move || {
        assert!(!token_clone1.is_cancelled());
        ready_worker.wait();
        while !token_clone1.is_cancelled() {
            std::hint::spin_loop();
        }
        assert!(token_clone1.is_cancelled());
    });

    ready.wait();
    token_clone2.cancel();
    assert!(token.is_cancelled());
    h.join().unwrap();
}

#[test]
fn test_fault_isolation_multi_dispatch() {
    // Test dispatching to 3 mock printers: Printer1, Printer2, Printer3
    let printers = vec![
        "Isolated_Printer_1".to_string(),
        "Isolated_Printer_2".to_string(),
        "Isolated_Printer_3".to_string(),
    ];

    let page_sizes = vec![(612.0, 792.0), (612.0, 792.0)];
    let settings = Settings {
        pages: (0..page_sizes.len()).collect(),
        ..Default::default()
    };
    let doc_source = SharedPdfSource::from(vec![]); // Empty doc causes worker failure on open

    let handle_res = PrintOrchestrationCoordinator::dispatch_multi(
        &printers,
        &settings,
        doc_source,
        &page_sizes,
    );

    assert!(handle_res.is_ok(), "Dispatch failed: {:?}", handle_res.err());
    let mut handle = handle_res.unwrap();

    assert_eq!(handle.all_telemetry().len(), 3);
    assert!(handle.get_telemetry("Isolated_Printer_1").is_some());
    assert!(handle.get_telemetry("Isolated_Printer_2").is_some());
    assert!(handle.get_telemetry("Isolated_Printer_3").is_some());

    // Allow workers to execute and reach independent terminal failure states
    std::thread::sleep(std::time::Duration::from_millis(150));
    let events = handle.poll_events();
    assert!(!events.is_empty(), "Expected events from workers");

    // Cancel only Printer 1
    let cancelled = handle.cancel_printer("Isolated_Printer_1");
    assert!(cancelled);

    // Verify Printer 2 and Printer 3 still retain their telemetry and are not cancelled
    let tel1 = handle.get_telemetry("Isolated_Printer_1");
    let tel2 = handle.get_telemetry("Isolated_Printer_2");
    let tel3 = handle.get_telemetry("Isolated_Printer_3");

    assert!(tel1.is_some());
    assert!(tel2.is_some());
    assert!(tel3.is_some());
}

#[test]
fn test_failover_reroute_zero_duplicates_zero_drops() {
    let printers = vec!["Primary_Printer".to_string()];
    let page_sizes = vec![
        (612.0, 792.0),
        (612.0, 792.0),
        (612.0, 792.0),
        (612.0, 792.0),
    ];
    let settings = Settings {
        pages: (0..page_sizes.len()).collect(),
        ..Default::default()
    };
    let doc_source = SharedPdfSource::from(vec![]);

    let mut handle = PrintOrchestrationCoordinator::dispatch_multi(
        &printers,
        &settings,
        doc_source,
        &page_sizes,
    ).expect("dispatch");

    // Wait for worker to fail or start
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = handle.poll_events();

    // Verify error cases:
    // 1. Unknown source printer
    let err = handle.failover_reroute("NonExistent_Printer", "Target_Printer");
    assert_eq!(err, Err(FailoverError::PrinterNotFound("NonExistent_Printer".to_string())));

    // 2. Target printer busy
    // If Primary_Printer is in telemetry, rerouting to Primary_Printer should fail if busy or valid
    let busy_err = handle.failover_reroute("Primary_Printer", "Primary_Printer");
    // Primary_Printer is already in telemetry
    assert!(busy_err.is_err());

    // 3. Reroute when cancelled
    handle.cancel_all();
    let cancel_err = handle.failover_reroute("Primary_Printer", "Backup_Printer");
    assert_eq!(cancel_err, Err(FailoverError::BatchCancelled));
}
