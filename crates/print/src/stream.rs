//! Page-by-page streaming pipeline with bounded backpressure (Feature 18)
//! and cooperative cancellation (Feature 19). Layer L4.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pdfcraft_cos::Document;
use crate::error::PrintError;
use crate::imposition;
use crate::settings::Settings;
use crate::sheets::{self, Sheet};

/// A serialized or rendered sheet ready for platform driver spooling.
#[derive(Debug, Clone)]
pub struct SheetBuffer {
    /// 0-based sheet index
    pub sheet_index: usize,
    /// Total number of sheets in the print job
    pub total_sheets: usize,
    /// Width of the sheet in PDF points (72 pt/in)
    pub width_pt: f64,
    /// Height of the sheet in PDF points (72 pt/in)
    pub height_pt: f64,
    /// Serialized sheet vector or raster data
    pub data: SheetData,
}

impl SheetBuffer {
    /// Approximate resident memory footprint of this buffer in bytes.
    pub fn resident_bytes(&self) -> usize {
        match &self.data {
            SheetData::Vector(v) => v.len(),
            SheetData::RasterTiles { rgba, .. } => rgba.len(),
        }
    }
}

/// The sheet's payload: vector commands or rasterized tiles.
#[derive(Debug, Clone)]
pub enum SheetData {
    /// Self-contained 1-sheet PDF vector document
    Vector(Vec<u8>),
    /// 300 DPI RGBA bitmap for GDI transparency flattening or image printing
    RasterTiles {
        width_px: u32,
        height_px: u32,
        dpi: f64,
        rgba: Vec<u8>,
    },
}

/// Cooperative cancellation token shared across UI, coordinator, and worker threads.
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

    /// Signals cancellation to all associated workers.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Queries whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Returns `Err(PrintError::Spool(...))` if cancelled.
    pub fn check_cancelled(&self) -> Result<(), PrintError> {
        if self.is_cancelled() {
            Err(PrintError::Spool("print job cancelled by operator".to_string()))
        } else {
            Ok(())
        }
    }
}

/// Bounded channel sender for sheet streaming with backpressure and cancellation support.
pub struct SheetSender {
    inner: std::sync::mpsc::SyncSender<SheetBuffer>,
}

/// Bounded channel receiver for sheet streaming.
pub struct SheetReceiver {
    inner: std::sync::mpsc::Receiver<SheetBuffer>,
}

/// Creates a bounded channel pair with the given capacity (recommended: 2).
pub fn bounded_stream_channel(capacity: usize) -> (SheetSender, SheetReceiver) {
    let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
    (SheetSender { inner: tx }, SheetReceiver { inner: rx })
}

impl SheetSender {
    /// Sends a sheet with backpressure. If the channel is full, pauses the thread
    /// and periodically checks the cancellation token.
    pub fn send_with_backpressure(
        &self,
        mut sheet: SheetBuffer,
        token: &CancellationToken,
    ) -> Result<(), PrintError> {
        loop {
            token.check_cancelled()?;

            match self.inner.try_send(sheet) {
                Ok(()) => return Ok(()),
                Err(std::sync::mpsc::TrySendError::Full(s)) => {
                    sheet = s;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Err(PrintError::Spool("spool consumer disconnected".to_string()));
                }
            }
        }
    }
}

impl SheetReceiver {
    /// Non-blocking check for the next sheet buffer.
    pub fn try_recv(&self) -> Result<Option<SheetBuffer>, PrintError> {
        match self.inner.try_recv() {
            Ok(sheet) => Ok(Some(sheet)),
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(PrintError::Spool("stream producer disconnected".to_string()))
            }
        }
    }

    /// Blocking receive with cancellation polling.
    pub fn recv_with_cancellation(&self, token: &CancellationToken) -> Result<Option<SheetBuffer>, PrintError> {
        loop {
            token.check_cancelled()?;
            match self.inner.try_recv() {
                Ok(sheet) => return Ok(Some(sheet)),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(None),
            }
        }
    }
}

/// Producer generating sheet buffers one by one on demand.
pub struct PageStreamProducer {
    source: Arc<Vec<u8>>,
    settings: Settings,
    cancellation: CancellationToken,
}

impl PageStreamProducer {
    pub fn new(source: Arc<Vec<u8>>, settings: Settings, cancellation: CancellationToken) -> Self {
        Self { source, settings, cancellation }
    }

    /// Runs the producer loop, emitting sheets into `sender` with backpressure.
    pub fn produce_all(self, sender: SheetSender) -> Result<usize, PrintError> {
        let doc = Document::open(self.source.clone()).map_err(PrintError::Cos)?;
        let pages = imposition::source_pages(&doc);
        let sizes: Vec<(f64, f64)> = pages.iter().map(|p| p.display_size(&doc)).collect();
        let sheets = sheets::layout(&sizes, &self.settings)?;
        let total_sheets = sheets.len();

        for (sheet_idx, sheet) in sheets.iter().enumerate() {
            self.cancellation.check_cancelled()?;

            let sheet_buf = self.produce_single_sheet(&doc, &pages, sheet, sheet_idx, total_sheets)?;
            sender.send_with_backpressure(sheet_buf, &self.cancellation)?;
        }

        Ok(total_sheets)
    }

    /// Imposes exactly one sheet into a self-contained 1-sheet buffer.
    fn produce_single_sheet(
        &self,
        doc: &Document,
        pages: &[pdfcraft_model::Page],
        sheet: &Sheet,
        sheet_idx: usize,
        total_sheets: usize,
    ) -> Result<SheetBuffer, PrintError> {
        let (width_pt, height_pt) = sheet.size;
        
        // Single sheet imposition: builds a 1-page document for this sheet
        let pdf_bytes = imposition::impose_single_sheet(doc, pages, sheet, &self.settings)?;

        Ok(SheetBuffer {
            sheet_index: sheet_idx,
            total_sheets,
            width_pt,
            height_pt,
            data: SheetData::Vector(pdf_bytes),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancellation_token_signaling() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        assert!(token.check_cancelled().is_ok());

        token.cancel();
        assert!(token.is_cancelled());
        assert!(token.check_cancelled().is_err());
    }

    #[test]
    fn test_bounded_channel_backpressure() {
        let (tx, rx) = bounded_stream_channel(2);
        let token = CancellationToken::new();

        let make_buf = |idx| SheetBuffer {
            sheet_index: idx,
            total_sheets: 10,
            width_pt: 595.0,
            height_pt: 842.0,
            data: SheetData::Vector(vec![0xAA; 128]),
        };

        // Fill channel to capacity (2)
        assert!(tx.send_with_backpressure(make_buf(0), &token).is_ok());
        assert!(tx.send_with_backpressure(make_buf(1), &token).is_ok());

        // Spawn consumer to drain after a short delay
        let consumer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let s0 = rx.recv_with_cancellation(&CancellationToken::new()).unwrap();
            let s1 = rx.recv_with_cancellation(&CancellationToken::new()).unwrap();
            let s2 = rx.recv_with_cancellation(&CancellationToken::new()).unwrap();
            (s0.unwrap().sheet_index, s1.unwrap().sheet_index, s2.unwrap().sheet_index)
        });

        // This third send will pause on backpressure until the consumer reads s0
        assert!(tx.send_with_backpressure(make_buf(2), &token).is_ok());

        let (i0, i1, i2) = consumer.join().unwrap();
        assert_eq!((i0, i1, i2), (0, 1, 2));
    }

    #[test]
    fn test_resident_bytes_calculation() {
        let vector_buf = SheetBuffer {
            sheet_index: 0,
            total_sheets: 1,
            width_pt: 612.0,
            height_pt: 792.0,
            data: SheetData::Vector(vec![0; 500]),
        };
        assert_eq!(vector_buf.resident_bytes(), 500);

        let raster_buf = SheetBuffer {
            sheet_index: 0,
            total_sheets: 1,
            width_pt: 612.0,
            height_pt: 792.0,
            data: SheetData::RasterTiles {
                width_px: 100,
                height_px: 100,
                dpi: 300.0,
                rgba: vec![0; 40_000],
            },
        };
        assert_eq!(raster_buf.resident_bytes(), 40_000);
    }
}
