//! Zero-Copy Shared Memory Document Snapshot (`SharedPdfSource`). Layer L6.
//!
//! Provides an immutable shared memory buffer backing concurrent printer workers,
//! preview generators, and background tasks. On native platforms, unmodified files
//! on disk are mapped directly via OS virtual memory (`memmap2::Mmap`), guaranteeing
//! $1.0\times$ RAM footprint regardless of concurrent worker count.
//! Unsaved document edits or dynamic sources are shared through `Arc<Vec<u8>>`.

#[cfg(not(target_arch = "wasm32"))]
use std::fs::File;
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::Arc;

/// Error conditions encountered when creating or accessing a `SharedPdfSource`.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("I/O error while opening or mapping file: {0}")]
    Io(#[from] std::io::Error),
    #[error("File is empty (0 bytes) and cannot be memory-mapped")]
    EmptyFile,
    #[error("Invalid slice range {range:?} for source of length {len}")]
    OutOfBounds { range: Range<usize>, len: usize },
}

/// Immutable shared memory snapshot of a PDF document backing concurrent print workers.
///
/// Can be backed by either an OS memory-mapped file (`memmap2::Mmap`) or a shared heap
/// vector (`Arc<Vec<u8>>`). Cloning is an $O(1)$ pointer increment and does not copy
/// the underlying buffer, guaranteeing $1.0\times$ memory footprint regardless of worker count.
#[derive(Clone)]
pub enum SharedPdfSource {
    /// Read-only memory-mapped file (backed by OS virtual memory page cache)
    #[cfg(not(target_arch = "wasm32"))]
    Mmap(Arc<memmap2::Mmap>),
    /// In-memory buffer for unsaved document state or non-file sources
    Heap(Arc<Vec<u8>>),
}

impl SharedPdfSource {
    /// Memory-maps a file from disk as a read-only document snapshot.
    ///
    /// # Safety Invariant
    /// The file is opened read-only and mapped into memory. The underlying file must
    /// not be truncated or deleted while the mapping is live.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(unsafe_code)]
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, SnapshotError> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 {
            return Err(SnapshotError::EmptyFile);
        }
        // SAFETY: The file is opened read-only and accessed strictly through immutable
        // slices (`&[u8]`). It is not mutated or truncated by this process.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Self::Mmap(Arc::new(mmap)))
    }

    /// Fallback for wasm32 where memory mapping is not supported.
    #[cfg(target_arch = "wasm32")]
    pub fn from_file<P: AsRef<Path>>(_path: P) -> Result<Self, SnapshotError> {
        Err(SnapshotError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Memory-mapped files are not supported on wasm32",
        )))
    }

    /// Creates a shared snapshot from an owned byte vector.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::Heap(Arc::new(bytes))
    }

    /// Creates a shared snapshot by reusing an existing `Arc<Vec<u8>>` without reallocation.
    pub fn from_arc(arc: Arc<Vec<u8>>) -> Self {
        Self::Heap(arc)
    }

    /// Returns `true` if the source is backed by a memory-mapped file.
    pub fn is_mmap(&self) -> bool {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Mmap(_) => true,
            Self::Heap(_) => false,
        }
    }

    /// Returns the total length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    /// Returns `true` if the source is empty (0 bytes).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }

    /// Returns a subslice of the underlying bytes if within bounds.
    #[inline]
    pub fn slice(&self, range: Range<usize>) -> Option<&[u8]> {
        self.as_ref().get(range)
    }

    /// Returns the underlying byte slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.as_ref()
    }

    /// Converts into a `pdfcraft_cos::Bytes` view.
    ///
    /// If backed by `Heap`, shares the underlying `Arc<Vec<u8>>` with 0 allocations.
    /// If backed by `Mmap`, copies the bytes into an owned vector.
    pub fn to_cos_bytes(&self) -> pdfcraft_cos::Bytes {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Mmap(m) => pdfcraft_cos::Bytes::from(m.as_ref().to_vec()),
            Self::Heap(h) => pdfcraft_cos::Bytes::from(h.clone()),
        }
    }
}

impl AsRef<[u8]> for SharedPdfSource {
    fn as_ref(&self) -> &[u8] {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Mmap(m) => &m[..],
            Self::Heap(h) => &h[..],
        }
    }
}

impl Deref for SharedPdfSource {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl From<Vec<u8>> for SharedPdfSource {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<Arc<Vec<u8>>> for SharedPdfSource {
    fn from(arc: Arc<Vec<u8>>) -> Self {
        Self::from_arc(arc)
    }
}

impl From<pdfcraft_cos::Bytes> for SharedPdfSource {
    fn from(b: pdfcraft_cos::Bytes) -> Self {
        Self::from_bytes(b.to_vec())
    }
}

impl PartialEq for SharedPdfSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            #[cfg(not(target_arch = "wasm32"))]
            (Self::Mmap(a), Self::Mmap(b)) if Arc::ptr_eq(a, b) => true,
            (Self::Heap(a), Self::Heap(b)) if Arc::ptr_eq(a, b) => true,
            _ => self.as_ref() == other.as_ref(),
        }
    }
}

impl Eq for SharedPdfSource {}

impl std::fmt::Debug for SharedPdfSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Mmap(m) => f
                .debug_struct("SharedPdfSource::Mmap")
                .field("len", &m.len())
                .finish(),
            Self::Heap(h) => f
                .debug_struct("SharedPdfSource::Heap")
                .field("len", &h.len())
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_bytes_as_ref() {
        let bytes = b"%PDF-1.7 hello world".to_vec();
        let source = SharedPdfSource::from_bytes(bytes.clone());
        assert_eq!(source.len(), bytes.len());
        assert_eq!(source.as_slice(), bytes.as_slice());
        assert_eq!(&*source, bytes.as_slice());
        assert!(!source.is_mmap());
        assert!(!source.is_empty());
    }

    #[test]
    fn test_from_arc_zero_copy() {
        let arc = Arc::new(b"%PDF-1.7 sample data".to_vec());
        let source = SharedPdfSource::from_arc(arc.clone());
        assert_eq!(source.as_ptr(), arc.as_ptr());
        assert_eq!(Arc::strong_count(&arc), 2);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn test_from_file_mmap() {
        let temp_path = std::env::temp_dir().join(format!("pdfcraft_test_mmap_{}_{}.pdf", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let payload = vec![0x42u8; 4096];
        std::fs::write(&temp_path, &payload).unwrap();

        let source = SharedPdfSource::from_file(&temp_path).unwrap();
        assert!(source.is_mmap());
        assert_eq!(source.len(), 4096);
        assert_eq!(source.as_slice(), payload.as_slice());
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn test_empty_file_rejected() {
        let temp_path = std::env::temp_dir().join(format!("pdfcraft_test_empty_{}_{}.pdf", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::write(&temp_path, b"").unwrap();
        let result = SharedPdfSource::from_file(&temp_path);
        assert!(matches!(result, Err(SnapshotError::EmptyFile)));
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_clone_is_zero_copy() {
        let source1 = SharedPdfSource::from_bytes(vec![1, 2, 3, 4, 5]);
        let source2 = source1.clone();
        assert_eq!(source1.as_ptr(), source2.as_ptr());
        assert_eq!(source1, source2);
    }

    #[test]
    fn test_slice_bounds() {
        let source = SharedPdfSource::from_bytes(b"0123456789".to_vec());
        assert_eq!(source.slice(2..6), Some(&b"2345"[..]));
        assert_eq!(source.slice(8..15), None);
    }

    #[test]
    fn test_concurrent_threads_reading() {
        let data = (0..10_000).map(|i| (i % 256) as u8).collect::<Vec<u8>>();
        let source = SharedPdfSource::from_bytes(data.clone());

        let mut handles = Vec::new();
        for t in 0..8 {
            let s = source.clone();
            let expected = data.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..100 {
                    let offset = (t * 100 + i) % 9000;
                    assert_eq!(&s[offset..offset + 100], &expected[offset..offset + 100]);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_to_cos_bytes() {
        let original = b"%PDF-1.4 test cos bytes".to_vec();
        let source = SharedPdfSource::from_bytes(original.clone());
        let cos_bytes = source.to_cos_bytes();
        assert_eq!(cos_bytes.as_ref(), original.as_slice());
    }
}
