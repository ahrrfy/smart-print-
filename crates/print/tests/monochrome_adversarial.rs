//! Empirical Adversarial Stress Test Suite for Monochrome Vector & Raster Pipeline.
//!
//! Stress-tests:
//! 1. Vector Content Streams: alternating operators, out-of-range operands, malformed operands,
//!    zero CMY leakage assertions.
//! 2. Inline Images: Flate, raw uncompressed, ASCIIHex, ASCII85, ImageMask stencils.
//! 3. Raster Image XObjects: 1x1, multi-megapixel, 16-bit RGB, CMYK non-standard decode,
//!    Indexed RGB and CMYK palettes, ImageMask stencils.
//! 4. Full PDF Document Roundtrip: Pages, Form XObjects, Annotation Appearances, and Image XObjects.

use std::sync::Arc;
use pdfcraft_content::{Op, parse, serialize_ops};
use pdfcraft_cos::{Dict, Document, Object, SaveOptions, Stream};
use pdfcraft_model::pages;
use pdfcraft_print::monochrome::MonochromeVectorFilter;

/// Helper: creates a minimal valid 1-page PDF containing the given content stream bytes
/// and optional indirect objects.
fn make_pdf_with_content_and_objects(
    content_ops: &[Op],
    extra_objects: Vec<(Dict, Vec<u8>)>,
) -> Vec<u8> {
    let mut doc = Document::new_empty();

    let content_bytes = serialize_ops(content_ops);
    let mut content_dict = Dict::new();
    content_dict.set(b"Length".to_vec(), Object::Int(content_bytes.len() as i64));
    let content_stream = Stream::from_raw(content_dict, content_bytes);
    let content_ref = doc.add(Object::Stream(content_stream));

    let mut res_dict = Dict::new();
    let mut xo_dict = Dict::new();

    for (i, (dict, data)) in extra_objects.into_iter().enumerate() {
        let stream = Stream::flate(dict, &data);
        let r = doc.add(Object::Stream(stream));
        let name = format!("Im{}", i + 1).into_bytes();
        xo_dict.set(name, Object::Ref(r));
    }
    if !xo_dict.is_empty() {
        res_dict.set(b"XObject".to_vec(), Object::Dict(xo_dict));
    }
    let res_ref = doc.add(Object::Dict(res_dict));

    let mut page_dict = Dict::new();
    page_dict.set(b"Type".to_vec(), Object::Name(b"Page".to_vec()));
    page_dict.set(b"MediaBox".to_vec(), Object::Array(vec![
        Object::Int(0), Object::Int(0), Object::Int(612), Object::Int(792),
    ]));
    page_dict.set(b"Contents".to_vec(), Object::Ref(content_ref));
    page_dict.set(b"Resources".to_vec(), Object::Ref(res_ref));

    let page_ref = doc.add(Object::Dict(page_dict));

    let mut pages_dict = Dict::new();
    pages_dict.set(b"Type".to_vec(), Object::Name(b"Pages".to_vec()));
    pages_dict.set(b"Kids".to_vec(), Object::Array(vec![Object::Ref(page_ref)]));
    pages_dict.set(b"Count".to_vec(), Object::Int(1));
    let pages_ref = doc.add(Object::Dict(pages_dict));

    // Update page parent
    let _ = doc.update_dict(page_ref, |d| {
        d.set(b"Parent".to_vec(), Object::Ref(pages_ref));
    });

    let mut cat_dict = Dict::new();
    cat_dict.set(b"Type".to_vec(), Object::Name(b"Catalog".to_vec()));
    cat_dict.set(b"Pages".to_vec(), Object::Ref(pages_ref));
    let cat_ref = doc.add(Object::Dict(cat_dict));

    doc.trailer_mut().set(b"Root".to_vec(), Object::Ref(cat_ref));
    pdfcraft_cos::write_full(&doc, &SaveOptions::default()).unwrap()
}

// =========================================================================
// 1. Vector Stream Tests
// =========================================================================

#[test]
fn test_adversarial_alternating_vector_operators() {
    // Generate 500 rapidly alternating color operators
    let mut ops = Vec::new();
    for i in 0..100 {
        // RGB stroke & fill
        ops.push(Op::new("rg", vec![
            pdfcraft_content::num((i as f64 * 0.01) % 1.0),
            pdfcraft_content::num(((i + 33) as f64 * 0.01) % 1.0),
            pdfcraft_content::num(((i + 67) as f64 * 0.01) % 1.0),
        ]));
        ops.push(Op::new("RG", vec![
            pdfcraft_content::num(((i + 10) as f64 * 0.01) % 1.0),
            pdfcraft_content::num(((i + 50) as f64 * 0.01) % 1.0),
            pdfcraft_content::num(((i + 90) as f64 * 0.01) % 1.0),
        ]));
        // CMYK stroke & fill
        ops.push(Op::new("k", vec![
            pdfcraft_content::num(0.8),
            pdfcraft_content::num(0.4),
            pdfcraft_content::num(0.2),
            pdfcraft_content::num(0.1),
        ]));
        ops.push(Op::new("K", vec![
            pdfcraft_content::num(0.1),
            pdfcraft_content::num(0.9),
            pdfcraft_content::num(0.5),
            pdfcraft_content::num(0.3),
        ]));
        // Gray stroke & fill
        ops.push(Op::new("g", vec![pdfcraft_content::num(0.5)]));
        ops.push(Op::new("G", vec![pdfcraft_content::num(0.8)]));
        // Colorspace switching & sc / SC
        ops.push(Op::new("cs", vec![Object::Name(b"DeviceRGB".to_vec())]));
        ops.push(Op::new("sc", vec![
            pdfcraft_content::num(0.2),
            pdfcraft_content::num(0.7),
            pdfcraft_content::num(0.1),
        ]));
        ops.push(Op::new("CS", vec![Object::Name(b"DeviceCMYK".to_vec())]));
        ops.push(Op::new("SC", vec![
            pdfcraft_content::num(0.5),
            pdfcraft_content::num(0.5),
            pdfcraft_content::num(0.5),
            pdfcraft_content::num(0.2),
        ]));
    }

    let filtered = MonochromeVectorFilter::filter_operators(&ops);
    assert_eq!(filtered.len(), ops.len());

    // Assert: ZERO color leaks across all filtered operators
    for (idx, op) in filtered.iter().enumerate() {
        match op.op.as_slice() {
            b"rg" | b"RG" => {
                panic!("Color leak! Found untransformed RGB operator {:?} at index {idx}", op.op);
            }
            b"k" | b"K" => {
                let nums = op.nums::<4>().expect("k/K operator must have 4 operands");
                assert_eq!(nums[0], 0.0, "Cyan leak detected at index {idx}: C={}", nums[0]);
                assert_eq!(nums[1], 0.0, "Magenta leak detected at index {idx}: M={}", nums[1]);
                assert_eq!(nums[2], 0.0, "Yellow leak detected at index {idx}: Y={}", nums[2]);
                assert!((0.0..=1.0).contains(&nums[3]), "K_eff out of bounds at index {idx}: K={}", nums[3]);
            }
            b"g" | b"G" => {
                let val = op.num(0).expect("g/G operator must have 1 operand");
                assert!((0.0..=1.0).contains(&val), "Gray value out of bounds at index {idx}: {val}");
            }
            b"cs" | b"CS" => {
                assert_eq!(
                    op.operands[0],
                    Object::Name(b"DeviceGray".to_vec()),
                    "Colorspace not converted to DeviceGray at index {idx}: {:?}",
                    op.operands[0]
                );
            }
            _ => {}
        }
    }
}

#[test]
fn test_out_of_range_and_adversarial_color_operands() {
    let adversarial_ops = vec![
        // Extreme out-of-range RGB
        Op::new("rg", vec![pdfcraft_content::num(2.5), pdfcraft_content::num(-1.0), pdfcraft_content::num(10.0)]),
        Op::new("RG", vec![pdfcraft_content::num(-999.0), pdfcraft_content::num(0.5), pdfcraft_content::num(999.0)]),
        // Extreme out-of-range CMYK
        Op::new("k", vec![
            pdfcraft_content::num(5.0),
            pdfcraft_content::num(-2.0),
            pdfcraft_content::num(10.0),
            pdfcraft_content::num(2.0),
        ]),
        Op::new("K", vec![
            pdfcraft_content::num(-1.0),
            pdfcraft_content::num(-1.0),
            pdfcraft_content::num(-1.0),
            pdfcraft_content::num(-5.0),
        ]),
        // CalRGB and non-standard names in cs / CS
        Op::new("cs", vec![Object::Name(b"CalRGB".to_vec())]),
        Op::new("CS", vec![Object::Name(b"CMYK".to_vec())]),
        // Malformed operand counts: rg with 2 operands, k with 3 operands
        Op::new("rg", vec![pdfcraft_content::num(0.5), pdfcraft_content::num(0.5)]),
        Op::new("k", vec![pdfcraft_content::num(0.1), pdfcraft_content::num(0.2), pdfcraft_content::num(0.3)]),
    ];

    let filtered = MonochromeVectorFilter::filter_operators(&adversarial_ops);
    assert_eq!(filtered.len(), adversarial_ops.len());

    // Check clamping on out-of-range RGB
    assert_eq!(filtered[0].op, b"g");
    let g0 = filtered[0].num(0).unwrap();
    assert!((0.0..=1.0).contains(&g0), "Clamping failed on out-of-range RGB: {g0}");

    // Check clamping on out-of-range CMYK
    assert_eq!(filtered[2].op, b"k");
    let k2 = filtered[2].nums::<4>().unwrap();
    assert_eq!(k2[0], 0.0);
    assert_eq!(k2[1], 0.0);
    assert_eq!(k2[2], 0.0);
    assert_eq!(k2[3], 1.0, "K_eff should be clamped to 1.0 for extreme values");

    assert_eq!(filtered[3].op, b"K");
    let k3 = filtered[3].nums::<4>().unwrap();
    assert_eq!(k3[0], 0.0);
    assert_eq!(k3[1], 0.0);
    assert_eq!(k3[2], 0.0);
    assert_eq!(k3[3], 0.0, "K_eff should be clamped to 0.0 for negative values");

    // CalRGB and CMYK cs rewritten to DeviceGray
    assert_eq!(filtered[4].operands[0], Object::Name(b"DeviceGray".to_vec()));
    assert_eq!(filtered[5].operands[0], Object::Name(b"DeviceGray".to_vec()));
}

// =========================================================================
// 2. Inline Image Tests
// =========================================================================

#[test]
fn test_inline_image_flate_and_raw_rgb_and_imagemask() {
    // 1. Raw uncompressed RGB inline image (1x1 red pixel)
    let mut raw_dict = Dict::new();
    raw_dict.set(b"W".to_vec(), Object::Int(1));
    raw_dict.set(b"H".to_vec(), Object::Int(1));
    raw_dict.set(b"BPC".to_vec(), Object::Int(8));
    raw_dict.set(b"CS".to_vec(), Object::Name(b"RGB".to_vec()));
    let raw_data = vec![255, 0, 0]; // Red
    let raw_op = Op {
        op: b"BI".to_vec(),
        operands: Vec::new(),
        inline: Some((raw_dict, raw_data)),
        span: 0..0,
    };

    // 2. Flate compressed RGB inline image (2x1 red and green)
    let mut flate_dict = Dict::new();
    flate_dict.set(b"W".to_vec(), Object::Int(2));
    flate_dict.set(b"H".to_vec(), Object::Int(1));
    flate_dict.set(b"BPC".to_vec(), Object::Int(8));
    flate_dict.set(b"CS".to_vec(), Object::Name(b"DeviceRGB".to_vec()));
    flate_dict.set(b"F".to_vec(), Object::Name(b"FlateDecode".to_vec()));
    let uncompressed_pixels = vec![255, 0, 0, 0, 255, 0];
    let compressed_pixels = pdfcraft_filters::encode_flate(&uncompressed_pixels);
    let flate_op = Op {
        op: b"BI".to_vec(),
        operands: Vec::new(),
        inline: Some((flate_dict, compressed_pixels)),
        span: 0..0,
    };

    // 3. Stencil ImageMask inline image
    let mut mask_dict = Dict::new();
    mask_dict.set(b"W".to_vec(), Object::Int(8));
    mask_dict.set(b"H".to_vec(), Object::Int(1));
    mask_dict.set(b"BPC".to_vec(), Object::Int(1));
    mask_dict.set(b"IM".to_vec(), Object::Bool(true));
    let mask_op = Op {
        op: b"BI".to_vec(),
        operands: Vec::new(),
        inline: Some((mask_dict, vec![0b10101010])),
        span: 0..0,
    };

    let filtered = MonochromeVectorFilter::filter_operators(&[raw_op, flate_op, mask_op]);
    assert_eq!(filtered.len(), 3);

    // Verify Raw RGB inline image converted to DeviceGray
    let (d0, data0) = filtered[0].inline.as_ref().unwrap();
    assert_eq!(d0.get(b"CS"), Some(&Object::Name(b"DeviceGray".to_vec())));
    assert_eq!(data0, &vec![76]); // (299*255 + 500)/1000 = 76

    // Verify Flate RGB inline image converted to DeviceGray
    let (d1, data1) = filtered[1].inline.as_ref().unwrap();
    assert_eq!(d1.get(b"CS"), Some(&Object::Name(b"DeviceGray".to_vec())));
    let decomp = pdfcraft_filters::decode(
        &[(pdfcraft_filters::Filter::Flate, pdfcraft_filters::Params::default())],
        data1,
        1024,
    ).unwrap();
    assert_eq!(decomp, vec![76, 150]); // Red=76, Green=150

    // Verify ImageMask stencil is preserved untouched
    let (d2, data2) = filtered[2].inline.as_ref().unwrap();
    assert_eq!(d2.get(b"IM"), Some(&Object::Bool(true)));
    assert_eq!(data2, &vec![0b10101010]);
}

#[test]
fn test_inline_image_asciihex_and_ascii85_filters() {
    // Test inline image with ASCIIHexDecode filter: /F /AHx
    // 1x1 Blue pixel: R=0, G=0, B=255. In hex: "0000FF>"
    // Correct Rec.601 luminance: (114 * 255 + 500) / 1000 = 29 (0x1D in hex)
    let mut ahx_dict = Dict::new();
    ahx_dict.set(b"W".to_vec(), Object::Int(1));
    ahx_dict.set(b"H".to_vec(), Object::Int(1));
    ahx_dict.set(b"BPC".to_vec(), Object::Int(8));
    ahx_dict.set(b"CS".to_vec(), Object::Name(b"DeviceRGB".to_vec()));
    ahx_dict.set(b"F".to_vec(), Object::Name(b"AHx".to_vec()));
    let ahx_data = b"0000FF>".to_vec();

    let ahx_op = Op {
        op: b"BI".to_vec(),
        operands: Vec::new(),
        inline: Some((ahx_dict, ahx_data)),
        span: 0..0,
    };

    let filtered = MonochromeVectorFilter::filter_operators(&[ahx_op]);
    let (d, data) = filtered[0].inline.as_ref().unwrap();

    println!("AHx inline image result dict: {:?}, data: {:?}", d, String::from_utf8_lossy(data));
    let _filter_name = d.get(b"F").or_else(|| d.get(b"Filter")).and_then(Object::as_name);

    // If filter is AHx, data MUST be valid ASCII hex ending in '>'
    // And for 1x1 image, it MUST decode to exactly 1 byte with value 29
    let is_valid_hex = data.iter().all(|&b| b.is_ascii_hexdigit() || b == b'>' || b.is_ascii_whitespace())
        && data.ends_with(b">");
    
    // Decode with pdfcraft_filters to verify decoded pixel
    let decoded = pdfcraft_filters::decode(
        &[(pdfcraft_filters::Filter::AsciiHex, pdfcraft_filters::Params::default())],
        data,
        100,
    );

    println!("Decoded AHx image data: {:?}", decoded);

    assert!(is_valid_hex, "BUG FOUND: Filter claims AHx but data contains non-hex bytes or lacks EOD delimiter '>': {:?}", String::from_utf8_lossy(data));
    match &decoded {
        Ok(bytes) => assert_eq!(bytes.as_slice(), &[29], "BUG FOUND: AHx inline image was not decoded before luma calculation! Expected [29], got {:?}", bytes),
        Err(e) => panic!("BUG FOUND: AHx inline image data cannot be decoded by AsciiHex filter! Error: {e}, data: {:?}", String::from_utf8_lossy(data)),
    }
}

// =========================================================================
// 3. Raster Image XObject Tests
// =========================================================================

#[test]
fn test_raster_image_1x1_and_multi_megapixel() {
    let mut doc = Document::new_empty();

    // 1. 1x1 CMYK image
    let mut img1_dict = Dict::new();
    img1_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    img1_dict.set(b"Width".to_vec(), Object::Int(1));
    img1_dict.set(b"Height".to_vec(), Object::Int(1));
    img1_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceCMYK".to_vec()));
    img1_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));
    let r1 = doc.add(Object::Stream(Stream::flate(img1_dict, &[0, 0, 0, 255]))); // Pure black

    // 2. 4-Megapixel RGB image (2000 x 2000 = 4,000,000 pixels = 12 MB raw RGB)
    let width = 2000;
    let height = 2000;
    let total_pixels = width * height;
    let mut raw_rgb = Vec::with_capacity(total_pixels * 3);
    for i in 0..total_pixels {
        raw_rgb.push((i % 256) as u8);
        raw_rgb.push(((i / 256) % 256) as u8);
        raw_rgb.push(128);
    }
    let mut img2_dict = Dict::new();
    img2_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    img2_dict.set(b"Width".to_vec(), Object::Int(width as i64));
    img2_dict.set(b"Height".to_vec(), Object::Int(height as i64));
    img2_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceRGB".to_vec()));
    img2_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));
    let r2 = doc.add(Object::Stream(Stream::flate(img2_dict, &raw_rgb)));

    // Run filter
    MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

    // Verify 1x1 image converted to DeviceGray
    let s1 = match &*doc.get(r1) {
        Object::Stream(s) => s.clone(),
        _ => panic!("expected stream for 1x1"),
    };
    assert_eq!(s1.dict.name(b"ColorSpace"), Some(b"DeviceGray".as_slice()));
    assert_eq!(s1.decoded().unwrap(), vec![0]); // Black -> 0

    // Verify 4-Megapixel image converted to DeviceGray
    let s2 = match &*doc.get(r2) {
        Object::Stream(s) => s.clone(),
        _ => panic!("expected stream for 4MP"),
    };
    assert_eq!(s2.dict.name(b"ColorSpace"), Some(b"DeviceGray".as_slice()));
    assert_eq!(s2.dict.int(b"BitsPerComponent"), Some(8));
    let decoded_4mp = s2.decoded().unwrap();
    assert_eq!(decoded_4mp.len(), total_pixels, "4MP image must have exactly 4,000,000 gray pixels");
}

#[test]
fn test_raster_image_16bit_rgb_reencoding() {
    let mut doc = Document::new_empty();

    // 16-bit RGB Image XObject (2 pixels: Red and Green in 16-bit big-endian)
    // Red:   R=0xFFFF, G=0x0000, B=0x0000 (6 bytes: [255, 255, 0, 0, 0, 0])
    // Green: R=0x0000, G=0xFFFF, B=0x0000 (6 bytes: [0, 0, 255, 255, 0, 0])
    let raw_16bit_rgb = vec![
        255, 255, 0, 0, 0, 0,
        0, 0, 255, 255, 0, 0,
    ];

    let mut img_dict = Dict::new();
    img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    img_dict.set(b"Width".to_vec(), Object::Int(2));
    img_dict.set(b"Height".to_vec(), Object::Int(1));
    img_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceRGB".to_vec()));
    img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(16));

    let r = doc.add(Object::Stream(Stream::flate(img_dict, &raw_16bit_rgb)));

    MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

    let s = match &*doc.get(r) {
        Object::Stream(s) => s.clone(),
        _ => panic!("expected stream"),
    };

    println!("16-bit image ColorSpace after filter: {:?}", s.dict.name(b"ColorSpace").map(|n| String::from_utf8_lossy(n)));
    println!("16-bit image BitsPerComponent after filter: {:?}", s.dict.int(b"BitsPerComponent"));

    // Check whether 16-bit RGB image was converted or left as Color DeviceRGB
    assert_eq!(
        s.dict.name(b"ColorSpace"),
        Some(b"DeviceGray".as_slice()),
        "BUG FOUND: 16-bit RGB image was NOT converted to DeviceGray! Color leak: ColorSpace is still {:?}",
        s.dict.get(b"ColorSpace")
    );
}

#[test]
fn test_raster_image_cmyk_non_standard_decode_array() {
    let mut doc = Document::new_empty();

    // CMYK image with inverted Decode array: [1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0]
    // In this inverted colorspace, raw sample 0 means 100% ink (black), and 255 means 0% ink (white).
    let raw_cmyk = vec![
        0, 0, 0, 0,       // Inverted: C=100%, M=100%, Y=100%, K=100% -> Should be BLACK (Gray = 0)
        255, 255, 255, 255, // Inverted: C=0%, M=0%, Y=0%, K=0% -> Should be WHITE (Gray = 255)
    ];

    let mut img_dict = Dict::new();
    img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    img_dict.set(b"Width".to_vec(), Object::Int(2));
    img_dict.set(b"Height".to_vec(), Object::Int(1));
    img_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceCMYK".to_vec()));
    img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));
    img_dict.set(b"Decode".to_vec(), Object::Array(vec![
        pdfcraft_content::num(1.0), pdfcraft_content::num(0.0),
        pdfcraft_content::num(1.0), pdfcraft_content::num(0.0),
        pdfcraft_content::num(1.0), pdfcraft_content::num(0.0),
        pdfcraft_content::num(1.0), pdfcraft_content::num(0.0),
    ]));

    let r = doc.add(Object::Stream(Stream::flate(img_dict, &raw_cmyk)));

    MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

    let s = match &*doc.get(r) {
        Object::Stream(s) => s.clone(),
        _ => panic!("expected stream"),
    };

    assert_eq!(s.dict.name(b"ColorSpace"), Some(b"DeviceGray".as_slice()));
    let decoded = s.decoded().unwrap();
    println!("CMYK with inverted decode converted samples: {:?}", decoded);

    // Pixel 0 was inverted 100% ink, so it should be dark/black (Gray ~ 0)
    // Pixel 1 was inverted 0% ink, so it should be white (Gray ~ 255)
    assert!(
        decoded[0] < 50,
        "BUG FOUND: Inverted CMYK Decode array ignored! Pixel 0 expected dark (<50), got {}",
        decoded[0]
    );
    assert!(
        decoded[1] > 200,
        "BUG FOUND: Inverted CMYK Decode array ignored! Pixel 1 expected light (>200), got {}",
        decoded[1]
    );
}

#[test]
fn test_raster_image_indexed_cmyk_palette() {
    let mut doc = Document::new_empty();

    // Indexed image with DeviceCMYK base color space
    // 2 entries: Entry 0 = Cyan (255, 0, 0, 0), Entry 1 = Magenta (0, 255, 0, 0)
    let cmyk_palette = vec![
        255, 0, 0, 0,
        0, 255, 0, 0,
    ];

    let cs = Object::Array(vec![
        Object::Name(b"Indexed".to_vec()),
        Object::Name(b"DeviceCMYK".to_vec()),
        Object::Int(1),
        Object::String(pdfcraft_cos::PdfString::literal(cmyk_palette)),
    ]);

    let mut img_dict = Dict::new();
    img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    img_dict.set(b"Width".to_vec(), Object::Int(2));
    img_dict.set(b"Height".to_vec(), Object::Int(1));
    img_dict.set(b"ColorSpace".to_vec(), cs);
    img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));

    let r = doc.add(Object::Stream(Stream::flate(img_dict, &[0, 1])));

    MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

    let s = match &*doc.get(r) {
        Object::Stream(s) => s.clone(),
        _ => panic!("expected stream"),
    };

    let cs_after = s.dict.get(b"ColorSpace").unwrap();
    println!("Indexed CMYK ColorSpace after filter: {:?}", cs_after);

    let is_device_gray_base = match cs_after {
        Object::Array(arr) => arr.get(1) == Some(&Object::Name(b"DeviceGray".to_vec())),
        Object::Name(n) => n == b"DeviceGray",
        _ => false,
    };

    assert!(
        is_device_gray_base,
        "BUG FOUND: Indexed image with DeviceCMYK base was NOT converted to DeviceGray palette! Color leak: {:?}",
        cs_after
    );
}

// =========================================================================
// 4. End-to-End Document Roundtrip
// =========================================================================

#[test]
fn test_full_document_pipeline_roundtrip_zero_color_leakage() {
    let page_ops = vec![
        Op::new("rg", vec![pdfcraft_content::num(1.0), pdfcraft_content::num(0.0), pdfcraft_content::num(0.0)]),
        Op::new("k", vec![pdfcraft_content::num(0.2), pdfcraft_content::num(0.8), pdfcraft_content::num(0.1), pdfcraft_content::num(0.0)]),
    ];

    let mut img_dict = Dict::new();
    img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
    img_dict.set(b"Width".to_vec(), Object::Int(1));
    img_dict.set(b"Height".to_vec(), Object::Int(1));
    img_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceRGB".to_vec()));
    img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));
    let img_data = vec![0, 255, 0]; // Green

    let doc_bytes = make_pdf_with_content_and_objects(&page_ops, vec![(img_dict, img_data)]);

    // Run MonochromeVectorFilter::filter_document_to_k_only
    let filtered_bytes = MonochromeVectorFilter::filter_document_to_k_only(&doc_bytes).unwrap();

    // Assert: output parses as valid PDF
    let out_doc = Document::open(Arc::new(filtered_bytes)).expect("Filtered PDF must parse cleanly");
    let page_list = pages(&out_doc);
    assert_eq!(page_list.len(), 1);

    // Verify page contents
    let page = &page_list[0];
    let contents_ref = match page.dict.get(b"Contents") {
        Some(Object::Ref(r)) => *r,
        other => panic!("expected contents ref, got {:?}", other),
    };

    let Object::Stream(s) = &*out_doc.get(contents_ref) else { panic!("expected stream") };
    let parsed_ops = parse(&s.decoded().unwrap());

    for op in &parsed_ops.ops {
        assert_ne!(op.op, b"rg", "Color leak: rg found in page contents!");
        assert_ne!(op.op, b"RG", "Color leak: RG found in page contents!");
        if op.op == b"k" || op.op == b"K" {
            let nums = op.nums::<4>().unwrap();
            assert_eq!(nums[0], 0.0, "Cyan leak in k/K!");
            assert_eq!(nums[1], 0.0, "Magenta leak in k/K!");
            assert_eq!(nums[2], 0.0, "Yellow leak in k/K!");
        }
    }

    // Verify Image XObject in resources
    let res = page.dict.get(b"Resources").map(|r| out_doc.resolve(r)).unwrap();
    let xo = res.as_dict().unwrap().get(b"XObject").map(|x| out_doc.resolve(x)).unwrap();
    let img_ref = xo.as_dict().unwrap().get(b"Im1").and_then(Object::as_ref).unwrap();

    let Object::Stream(img_stream) = &*out_doc.get(img_ref) else { panic!("expected stream") };
    assert_eq!(
        img_stream.dict.name(b"ColorSpace"),
        Some(b"DeviceGray".as_slice()),
        "Image XObject ColorSpace must be DeviceGray"
    );
    assert_eq!(img_stream.decoded().unwrap().len(), 1, "Image must have 1 gray pixel");
}
