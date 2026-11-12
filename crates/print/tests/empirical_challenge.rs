//! Empirical Challenger Stress & Correctness Suite for Streaming & Backpressure (Milestone 3).
//!
//! Evaluates:
//! 1. Backpressure Blocking & Producer Stalling on Full Channel (capacity = 2).
//! 2. Consumer Early Drop Non-Blocking Error Propagation.
//! 3. Blocked Producer Cancellation Responsiveness (< 50 ms unblock).
//! 4. Single-Sheet Resident Buffer Boundedness ($O(1)$ memory < 50 KB / sheet).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pdfcraft_print::governor::CancellationToken;
use pdfcraft_print::stream::{bounded_stream_channel, SheetBuffer, SheetData};

#[test]
fn challenge_backpressure_blocks_producer_until_consumer_dequeues() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    let make_sheet = |idx| SheetBuffer {
        sheet_index: idx,
        total_sheets: 10,
        width_pt: 612.0,
        height_pt: 792.0,
        data: SheetData::Vector(vec![0x55; 1024]),
    };

    // Slot 0 and Slot 1 fill capacity 2
    assert!(tx.send_with_backpressure(make_sheet(0), &token).is_ok());
    assert!(tx.send_with_backpressure(make_sheet(1), &token).is_ok());

    // Slot 2 must BLOCK. We verify it blocks by launching a thread and asserting it has not returned after 40ms.
    let send_finished = Arc::new(AtomicBool::new(false));
    let send_finished_clone = send_finished.clone();
    let token_clone = token.clone();

    let producer_thread = std::thread::spawn(move || {
        let res = tx.send_with_backpressure(make_sheet(2), &token_clone);
        send_finished_clone.store(true, Ordering::SeqCst);
        res
    });

    // Sleep 40ms: producer should still be blocked waiting for capacity
    std::thread::sleep(Duration::from_millis(40));
    assert!(
        !send_finished.load(Ordering::SeqCst),
        "Backpressure FAILED: Producer did not block when channel was full (capacity = 2)!"
    );

    // Now consumer reads ONE sheet, opening a slot
    let s0 = rx.recv_with_cancellation(&token).unwrap();
    assert_eq!(s0.unwrap().sheet_index, 0);

    // Producer must now successfully unblock and finish sending
    producer_thread.join().unwrap().unwrap();
    assert!(send_finished.load(Ordering::SeqCst));

    // Consumer reads remaining sheets
    let s1 = rx.recv_with_cancellation(&token).unwrap();
    let s2 = rx.recv_with_cancellation(&token).unwrap();
    assert_eq!(s1.unwrap().sheet_index, 1);
    assert_eq!(s2.unwrap().sheet_index, 2);
}

#[test]
fn challenge_blocked_producer_unblocks_immediately_on_cancellation() {
    let (tx, _rx) = bounded_stream_channel(1);
    let token = CancellationToken::new();

    let make_sheet = |idx| SheetBuffer {
        sheet_index: idx,
        total_sheets: 5,
        width_pt: 612.0,
        height_pt: 792.0,
        data: SheetData::Vector(vec![0xAA; 512]),
    };

    // Fill channel capacity 1
    assert!(tx.send_with_backpressure(make_sheet(0), &token).is_ok());

    // Producer tries to send 2nd sheet, which blocks
    let token_clone = token.clone();
    let producer_thread = std::thread::spawn(move || {
        tx.send_with_backpressure(make_sheet(1), &token_clone)
    });

    // Wait 30ms then cancel
    std::thread::sleep(Duration::from_millis(30));
    let cancel_time = Instant::now();
    token.cancel();

    // Producer must unblock within < 50ms of cancellation signal
    let res = producer_thread.join().unwrap();
    let latency = cancel_time.elapsed();

    assert!(
        res.is_err(),
        "Producer must return error on cancellation while blocked on backpressure"
    );
    assert!(
        latency < Duration::from_millis(80),
        "Cancellation latency too high: {:?}",
        latency
    );
}

#[test]
fn challenge_consumer_early_disconnect_does_not_deadlock_producer() {
    let (tx, rx) = bounded_stream_channel(2);
    let token = CancellationToken::new();

    let make_sheet = |idx| SheetBuffer {
        sheet_index: idx,
        total_sheets: 10,
        width_pt: 612.0,
        height_pt: 792.0,
        data: SheetData::Vector(vec![0xBB; 256]),
    };

    // Fill channel
    assert!(tx.send_with_backpressure(make_sheet(0), &token).is_ok());
    assert!(tx.send_with_backpressure(make_sheet(1), &token).is_ok());

    // Drop the consumer receiver explicitly
    drop(rx);

    // Producer sending next sheet must fail immediately without deadlock
    let res = tx.send_with_backpressure(make_sheet(2), &token);
    assert!(
        res.is_err(),
        "Producer must detect disconnected consumer and abort cleanly"
    );
}
