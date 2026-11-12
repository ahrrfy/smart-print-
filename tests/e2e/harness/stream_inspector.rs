use flate2::read::ZlibDecoder;
use std::fmt;
use std::io::Read;
use std::ops::Range;

/// Exact violation detected in a PDF content stream or resource dictionary.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamViolation {
    /// Found forbidden RGB operator `rg` or `RG` with operands [r, g, b]
    RgbColorOperator {
        op: String,
        r: f64,
        g: f64,
        b: f64,
        page_index: usize,
        byte_span: Range<usize>,
    },
    /// Found CMYK operator `k` or `K` with non-zero Cyan, Magenta, or Yellow components
    CmykChromaticColor {
        op: String,
        c: f64,
        m: f64,
        y: f64,
        k: f64,
        page_index: usize,
        byte_span: Range<usize>,
    },
    /// Found forbidden ColorSpace declaration (`/DeviceRGB` or `/DeviceCMYK`)
    ForbiddenColorSpace {
        color_space: String,
        context: ColorSpaceContext,
        page_index: usize,
    },
    /// Image XObject is not true grayscale
    InvalidImageXObject {
        xobject_name: String,
        color_space: String,
        bits_per_component: usize,
        channels_detected: usize,
        page_index: usize,
    },
    /// Inline image (`BI`...`ID`...`EI`) has color configuration
    InvalidInlineImage {
        color_space: String,
        page_index: usize,
        byte_offset: usize,
    },
    /// Stream syntax error or corruption encountered during parsing
    MalformedStream {
        page_index: usize,
        details: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorSpaceContext {
    ResourceDict,
    OperatorCs,
    OperatorCS,
    InlineImage,
    ShadingDict,
}

impl fmt::Display for StreamViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RgbColorOperator {
                op,
                r,
                g,
                b,
                page_index,
                byte_span,
            } => {
                write!(
                    f,
                    "Page {}: forbidden RGB operator '{}' [{:.3}, {:.3}, {:.3}] at span {:?}",
                    page_index + 1,
                    op,
                    r,
                    g,
                    b,
                    byte_span
                )
            }
            Self::CmykChromaticColor {
                op,
                c,
                m,
                y,
                k,
                page_index,
                byte_span,
            } => {
                write!(
                    f,
                    "Page {}: non-zero chromatic CMYK operator '{}' [C:{:.3}, M:{:.3}, Y:{:.3}, K:{:.3}] at span {:?}",
                    page_index + 1,
                    op,
                    c,
                    m,
                    y,
                    k,
                    byte_span
                )
            }
            Self::ForbiddenColorSpace {
                color_space,
                context,
                page_index,
            } => {
                write!(
                    f,
                    "Page {}: forbidden color space '{}' in {:?}",
                    page_index + 1,
                    color_space,
                    context
                )
            }
            Self::InvalidImageXObject {
                xobject_name,
                color_space,
                bits_per_component,
                channels_detected,
                page_index,
            } => {
                write!(
                    f,
                    "Page {}: Image XObject '{}' has ColorSpace '{}' (BPC: {}, channels: {})",
                    page_index + 1,
                    xobject_name,
                    color_space,
                    bits_per_component,
                    channels_detected
                )
            }
            Self::InvalidInlineImage {
                color_space,
                page_index,
                byte_offset,
            } => {
                write!(
                    f,
                    "Page {}: inline image with color space '{}' at byte {}",
                    page_index + 1,
                    color_space,
                    byte_offset
                )
            }
            Self::MalformedStream {
                page_index,
                details,
            } => {
                write!(f, "Page {}: malformed stream: {}", page_index + 1, details)
            }
        }
    }
}

/// Comprehensive inspection summary report.
#[derive(Debug, Clone, Default)]
pub struct KOnlyInspectionReport {
    pub total_pages: usize,
    pub total_content_streams: usize,
    pub total_operators_parsed: usize,
    pub gray_operator_count: usize, // count of `g` and `G`
    pub k_only_cmyk_count: usize,   // count of `k` and `K` with C=0, M=0, Y=0
    pub image_xobjects_inspected: usize,
    pub inline_images_inspected: usize,
    pub violations: Vec<StreamViolation>,
}

impl KOnlyInspectionReport {
    pub fn is_pure_k_only(&self) -> bool {
        self.violations.is_empty()
    }
}

pub trait PdfStreamInspector {
    fn inspect_pdf_document(&self, pdf_bytes: &[u8]) -> Result<KOnlyInspectionReport, InspectorError>;
    fn inspect_content_stream(&self, stream_bytes: &[u8], page_index: usize) -> Vec<StreamViolation>;
    fn inspect_image_xobject(
        &self,
        name: &str,
        dict_text: &str,
        raw_pixels: &[u8],
        page_index: usize,
    ) -> Result<(), StreamViolation>;
}

#[derive(Debug, thiserror::Error)]
pub enum InspectorError {
    #[error("Failed to parse PDF document structure: {0}")]
    PdfParseFailed(String),
    #[error("Failed to decompress content stream: {0}")]
    StreamDecompressionFailed(String),
    #[error("I/O error during inspection: {0}")]
    Io(#[from] std::io::Error),
}

/// Default stream inspection implementation.
#[derive(Default)]
pub struct DefaultStreamInspector;

impl DefaultStreamInspector {
    pub fn new() -> Self {
        Self
    }

    /// Try to decompress Flate/zlib data if compressed; otherwise returns original slice.
    pub fn try_decompress(bytes: &[u8]) -> Vec<u8> {
        let mut decoder = ZlibDecoder::new(bytes);
        let mut out = Vec::new();
        if decoder.read_to_end(&mut out).is_ok() && !out.is_empty() {
            out
        } else {
            bytes.to_vec()
        }
    }
}

impl PdfStreamInspector for DefaultStreamInspector {
    fn inspect_content_stream(&self, stream_bytes: &[u8], page_index: usize) -> Vec<StreamViolation> {
        let text = String::from_utf8_lossy(stream_bytes);
        let mut violations = Vec::new();

        // Tokenize stream by whitespace and analyze operator patterns
        let tokens: Vec<&str> = text.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            let tok = tokens[i];

            match tok {
                "rg" | "RG" => {
                    // Preceded by 3 operands: r g b
                    let (r, g, b) = if i >= 3 {
                        let r = tokens[i - 3].parse::<f64>().unwrap_or(0.0);
                        let g = tokens[i - 2].parse::<f64>().unwrap_or(0.0);
                        let b = tokens[i - 1].parse::<f64>().unwrap_or(0.0);
                        (r, g, b)
                    } else {
                        (0.0, 0.0, 0.0)
                    };

                    violations.push(StreamViolation::RgbColorOperator {
                        op: tok.to_string(),
                        r,
                        g,
                        b,
                        page_index,
                        byte_span: i..i + 1,
                    });
                }
                "k" | "K" if i >= 4 => {
                    // Preceded by 4 operands: c m y k
                    let c = tokens[i - 4].parse::<f64>().unwrap_or(0.0);
                    let m = tokens[i - 3].parse::<f64>().unwrap_or(0.0);
                    let y = tokens[i - 2].parse::<f64>().unwrap_or(0.0);
                    let k = tokens[i - 1].parse::<f64>().unwrap_or(0.0);

                    // True K-only requires c == 0, m == 0, y == 0 within floating tolerance
                    const EPSILON: f64 = 1e-4;
                    if c.abs() > EPSILON || m.abs() > EPSILON || y.abs() > EPSILON {
                        violations.push(StreamViolation::CmykChromaticColor {
                            op: tok.to_string(),
                            c,
                            m,
                            y,
                            k,
                            page_index,
                            byte_span: i..i + 1,
                        });
                    }
                }
                "cs" | "CS" if i >= 1 => {
                    let cs = tokens[i - 1];
                    if cs.contains("DeviceRGB") || cs.contains("DeviceCMYK") {
                        violations.push(StreamViolation::ForbiddenColorSpace {
                            color_space: cs.to_string(),
                            context: if tok == "cs" {
                                ColorSpaceContext::OperatorCs
                            } else {
                                ColorSpaceContext::OperatorCS
                            },
                            page_index,
                        });
                    }
                }
                "/DeviceRGB" | "/DeviceCMYK" => {
                    violations.push(StreamViolation::ForbiddenColorSpace {
                        color_space: tok.to_string(),
                        context: ColorSpaceContext::ResourceDict,
                        page_index,
                    });
                }
                _ => {}
            }
            i += 1;
        }

        violations
    }

    fn inspect_image_xobject(
        &self,
        name: &str,
        dict_text: &str,
        raw_pixels: &[u8],
        page_index: usize,
    ) -> Result<(), StreamViolation> {
        // Look for /ColorSpace in dictionary
        let is_gray = dict_text.contains("/DeviceGray")
            || dict_text.contains("/CalGray")
            || dict_text.contains("/G");
        let is_rgb = dict_text.contains("/DeviceRGB");
        let is_cmyk = dict_text.contains("/DeviceCMYK");

        if is_rgb || is_cmyk || !is_gray {
            let cs_name = if is_rgb {
                "/DeviceRGB"
            } else if is_cmyk {
                "/DeviceCMYK"
            } else {
                "Unknown"
            };
            return Err(StreamViolation::InvalidImageXObject {
                xobject_name: name.to_string(),
                color_space: cs_name.to_string(),
                bits_per_component: 8,
                channels_detected: if is_cmyk { 4 } else { 3 },
                page_index,
            });
        }

        // If raw pixel bytes are provided, verify pixel stride (1 byte per pixel for 8bpc)
        // Extract width and height if present in dict
        let mut width: usize = 0;
        let mut height: usize = 0;
        let mut has_width = false;
        let mut has_height = false;
        let tokens: Vec<&str> = dict_text.split_whitespace().collect();
        for i in 0..tokens.len() {
            if tokens[i] == "/Width" && i + 1 < tokens.len() {
                if let Ok(w) = tokens[i + 1].parse::<usize>() {
                    width = w;
                    has_width = true;
                }
            } else if let Some(rest) = tokens[i].strip_prefix("/Width")
                && let Ok(w) = rest.trim().parse::<usize>()
            {
                width = w;
                has_width = true;
            }
            if tokens[i] == "/Height" && i + 1 < tokens.len() {
                if let Ok(h) = tokens[i + 1].parse::<usize>() {
                    height = h;
                    has_height = true;
                }
            } else if let Some(rest) = tokens[i].strip_prefix("/Height")
                && let Ok(h) = rest.trim().parse::<usize>()
            {
                height = h;
                has_height = true;
            }
        }

        // Boundary rejection: Zero dimension width or height is invalid
        if (has_width && width == 0) || (has_height && height == 0) {
            return Err(StreamViolation::MalformedStream {
                page_index,
                details: format!("Image XObject '{name}' has illegal zero dimension: width={width}, height={height}"),
            });
        }

        // Decompress FlateDecode stream if specified and verify integrity
        let decompressed_buf;
        let effective_pixels = if dict_text.contains("/FlateDecode") && !raw_pixels.is_empty() {
            let mut decoder = ZlibDecoder::new(raw_pixels);
            let mut out = Vec::new();
            if let Err(e) = decoder.read_to_end(&mut out) {
                return Err(StreamViolation::MalformedStream {
                    page_index,
                    details: format!("Image XObject '{name}' corrupted Flate stream: {e}"),
                });
            }
            decompressed_buf = out;
            &decompressed_buf[..]
        } else {
            raw_pixels
        };

        if width > 0 && height > 0 && !effective_pixels.is_empty() {
            let expected_gray_bytes = width * height;
            if effective_pixels.len() == expected_gray_bytes * 3 || effective_pixels.len() == expected_gray_bytes * 4 {
                return Err(StreamViolation::InvalidImageXObject {
                    xobject_name: name.to_string(),
                    color_space: "/DeviceGray".to_string(),
                    bits_per_component: 8,
                    channels_detected: effective_pixels.len() / (width * height),
                    page_index,
                });
            }
        }

        Ok(())
    }

    fn inspect_pdf_document(&self, pdf_bytes: &[u8]) -> Result<KOnlyInspectionReport, InspectorError> {
        let mut report = KOnlyInspectionReport::default();
        let text = String::from_utf8_lossy(pdf_bytes);

        // Find stream ... endstream blocks
        let mut cursor = 0;
        let mut stream_count = 0;

        while let Some(start_stream) = text[cursor..].find("stream") {
            let stream_pos = cursor + start_stream + 6;
            // Handle optional \r\n or \n following stream keyword
            let data_start = if pdf_bytes.get(stream_pos) == Some(&b'\r')
                && pdf_bytes.get(stream_pos + 1) == Some(&b'\n')
            {
                stream_pos + 2
            } else if pdf_bytes.get(stream_pos) == Some(&b'\n') {
                stream_pos + 1
            } else {
                stream_pos
            };

            if let Some(end_stream) = text[data_start..].find("endstream") {
                let data_end = data_start + end_stream;
                let stream_slice = &pdf_bytes[data_start..data_end];
                let decompressed = Self::try_decompress(stream_slice);

                let page_idx = stream_count; // Approximate page mapping
                let violations = self.inspect_content_stream(&decompressed, page_idx);

                // Count gray and k-only operators
                let stream_text = String::from_utf8_lossy(&decompressed);
                for tok in stream_text.split_whitespace() {
                    report.total_operators_parsed += 1;
                    if tok == "g" || tok == "G" {
                        report.gray_operator_count += 1;
                    } else if tok == "k" || tok == "K" {
                        report.k_only_cmyk_count += 1;
                    }
                }

                report.violations.extend(violations);
                stream_count += 1;
                cursor = data_end + 9;
            } else {
                break;
            }
        }

        // Also check if any uncompressed /DeviceRGB or /DeviceCMYK appear in resource dicts
        for line in text.lines() {
            if line.contains("/DeviceRGB") || line.contains("/DeviceCMYK") {
                // Ensure it's not already captured
                let cs = if line.contains("/DeviceRGB") {
                    "/DeviceRGB"
                } else {
                    "/DeviceCMYK"
                };
                if !report
                    .violations
                    .iter()
                    .any(|v| matches!(v, StreamViolation::ForbiddenColorSpace { color_space, .. } if color_space == cs))
                {
                    report.violations.push(StreamViolation::ForbiddenColorSpace {
                        color_space: cs.to_string(),
                        context: ColorSpaceContext::ResourceDict,
                        page_index: 0,
                    });
                }
            }
        }

        report.total_pages = stream_count.max(1);
        report.total_content_streams = stream_count;
        Ok(report)
    }
}

pub struct StreamAssertions;

impl StreamAssertions {
    /// Assert that the given PDF bytes are strictly pure K-only monochrome.
    /// Panics with a formatted diagnostic report if any color violations are found.
    pub fn assert_true_k_only(pdf_bytes: &[u8]) {
        let inspector = DefaultStreamInspector::new();
        let report = inspector
            .inspect_pdf_document(pdf_bytes)
            .expect("PDF parsing failed during stream assertion");

        if !report.is_pure_k_only() {
            let mut message = format!(
                "\n================= STREAM VIOLATION REPORT =================\n\
                 Detected {} color violation(s) across {} page(s):\n",
                report.violations.len(),
                report.total_pages
            );
            for (idx, violation) in report.violations.iter().enumerate() {
                message.push_str(&format!("  [{}] {}\n", idx + 1, violation));
            }
            message.push_str("===========================================================\n");
            panic!("{}", message);
        }
    }

    /// Assert that a raw content stream contains zero color operators.
    pub fn assert_content_stream_k_only(stream_bytes: &[u8]) {
        let inspector = DefaultStreamInspector::new();
        let violations = inspector.inspect_content_stream(stream_bytes, 0);
        assert!(
            violations.is_empty(),
            "Raw content stream contained color violations: {:?}",
            violations
        );
    }
}
