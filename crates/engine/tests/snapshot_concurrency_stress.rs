//! Empirical Challenger Test for SharedPdfSource Zero-Copy Multi-Threaded Invariance.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use pdfcraft_engine::snapshot::SharedPdfSource;

#[test]
fn challenge_zero_copy_multi_threaded_pointer_identity() {
    // 5 MB random byte payload
    let payload_size = 5 * 1024 * 1024;
    let original_data: Vec<u8> = (0..payload_size).map(|i| (i % 251) as u8).collect();
    let source = SharedPdfSource::from_bytes(original_data);

    let base_ptr = source.as_ptr() as usize;
    let thread_count = 16;
    let completed = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for thread_idx in 0..thread_count {
        let s = source.clone();
        let comp = completed.clone();
        handles.push(std::thread::spawn(move || {
            // Assert that cloned source in thread has EXACT same memory address (0 bytes copied)
            let thread_ptr = s.as_ptr() as usize;
            assert_eq!(
                thread_ptr, base_ptr,
                "Thread {} allocated duplicate memory buffer instead of zero-copy sharing!",
                thread_idx
            );

            // Read various offsets concurrently
            for i in 0..500 {
                let offset = (thread_idx * 10_000 + i * 17) % (payload_size - 100);
                let slice = &s[offset..offset + 100];
                assert_eq!(slice[0], (offset % 251) as u8);
            }

            comp.fetch_add(1, Ordering::SeqCst);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(completed.load(Ordering::SeqCst), thread_count);
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn challenge_mmap_zero_copy_pointer_identity_across_threads() {
    let temp_file = std::env::temp_dir().join(format!("pdfcraft_mmap_stress_{}.bin", std::process::id()));
    let payload = vec![0x37u8; 2 * 1024 * 1024]; // 2 MB file
    std::fs::write(&temp_file, &payload).unwrap();

    let mmap_source = SharedPdfSource::from_file(&temp_file).unwrap();
    assert!(mmap_source.is_mmap());
    let mmap_base_ptr = mmap_source.as_ptr() as usize;

    let mut handles = Vec::new();
    for thread_idx in 0..8 {
        let s = mmap_source.clone();
        handles.push(std::thread::spawn(move || {
            let thread_ptr = s.as_ptr() as usize;
            assert_eq!(
                thread_ptr, mmap_base_ptr,
                "Thread {} duplicated memory-mapped file pages in RAM!",
                thread_idx
            );
            assert_eq!(s[1024], 0x37);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let _ = std::fs::remove_file(&temp_file);
}
