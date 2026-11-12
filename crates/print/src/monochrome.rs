//! In-memory vector content stream K-only grayscale filter and Image XObject luminance re-encoding.
//!
//! Enforces Layer 2 of the Dual-Layer Monochrome Protection architecture:
//! - Rewrites vector operators (`rg`, `RG`, `k`, `K`, `cs`, `CS`, `sc`, `SC`, `scn`, `SCN`) to `/DeviceGray`
//!   or `0 0 0 K_eff` using standard Rec. 601 integer math to guarantee 0% Cyan, Magenta, or Yellow ink/toner
//!   consumption on physical color devices.
//! - Re-encodes inline images (`BI ... ID ... EI`) to 8-bit `/DeviceGray`.
//! - Traverses all Image XObjects via global object number discovery and re-encodes RGB/CMYK/Indexed
//!   raster images to 8-bit `/DeviceGray` luminance with Flate compression and normalized dictionaries.

use std::collections::HashSet;
use std::sync::Arc;

use pdfcraft_content::{Op, parse, serialize_ops};
use pdfcraft_cos::{CosError, Dict, Document, ObjRef, Object, PdfString, SaveOptions, Stream};
use pdfcraft_model::pages;

use crate::error::PrintError;

pub struct MonochromeVectorFilter;

impl MonochromeVectorFilter {
    /// In-memory transformation of PDF content streams and Image XObjects to true K-only.
    /// Operands (`rg`, `RG`, `k`, `K`, `cs`, `CS`, `sc`, `SC`) are translated to `/DeviceGray` or `0 0 0 K_eff`.
    /// Raster Image XObjects are re-encoded to 8-bit `/DeviceGray` luminance.
    pub fn filter_document_to_k_only(doc_bytes: &[u8]) -> Result<Vec<u8>, PrintError> {
        let mut doc = Document::open(Arc::new(doc_bytes.to_vec()))?;
        let mut visited_streams: HashSet<ObjRef> = HashSet::new();

        // 1. Process all pages
        let page_list = pages(&doc);
        for page in &page_list {
            // A. Rewrite Page /Contents
            Self::filter_page_contents(&mut doc, page.obj, &page.dict, &mut visited_streams)?;

            // B. Rewrite Transparency Group /CS to DeviceGray
            if let Some(group_obj) = page.dict.get(b"Group") {
                Self::filter_transparency_group(&mut doc, page.obj, group_obj);
            }

            // C. Recursively process Form XObjects in /Resources
            if let Some(res) = page.dict.get(b"Resources") {
                Self::filter_resources(&mut doc, res, &mut visited_streams)?;
            }

            // D. Process Annotation Appearance Streams in /Annots
            if let Some(annots) = page.dict.get(b"Annots") {
                Self::filter_annots(&mut doc, annots, &mut visited_streams)?;
            }
        }

        // 2. Perform Image XObject grayscale re-encoding globally across all indirect objects
        Self::filter_image_xobjects(&mut doc)?;

        // 3. Serialize transformed document to in-memory bytes
        let out_bytes = pdfcraft_cos::write_full(&doc, &SaveOptions::default())?;
        Ok(out_bytes)
    }

    /// Resolves an indirect reference to a stream object, following any reference hops.
    fn resolve_stream_ref(doc: &Document, mut r: ObjRef) -> Option<ObjRef> {
        for _ in 0..10 {
            match &*doc.get(r) {
                Object::Stream(_) => return Some(r),
                Object::Ref(next_r) => r = *next_r,
                _ => return None,
            }
        }
        None
    }

    /// Filters page content streams.
    fn filter_page_contents(
        doc: &mut Document,
        _page_ref: ObjRef,
        page_dict: &Dict,
        visited: &mut HashSet<ObjRef>,
    ) -> Result<(), PrintError> {
        let Some(contents_obj) = page_dict.get(b"Contents") else {
            return Ok(());
        };

        let stream_refs: Vec<ObjRef> = match contents_obj {
            Object::Ref(r) => {
                let resolved = doc.resolve(contents_obj);
                if let Object::Array(list) = &*resolved {
                    list.iter()
                        .filter_map(|e| e.as_ref().and_then(|r| Self::resolve_stream_ref(doc, r)))
                        .collect()
                } else {
                    Self::resolve_stream_ref(doc, *r).into_iter().collect()
                }
            }
            Object::Array(list) => {
                list.iter()
                    .filter_map(|e| e.as_ref().and_then(|r| Self::resolve_stream_ref(doc, r)))
                    .collect()
            }
            _ => Vec::new(),
        };

        for r in stream_refs {
            if !visited.insert(r) {
                continue;
            }
            let Object::Stream(s) = &*doc.get(r) else {
                continue;
            };
            let Ok(decoded_data) = s.decoded() else {
                continue;
            };
            let parsed = parse(&decoded_data);
            let filtered_ops = Self::filter_operators(&parsed.ops);
            let new_data = serialize_ops(&filtered_ops);

            let mut new_dict = s.dict.clone();
            new_dict.remove(b"Length");
            new_dict.remove(b"Filter");
            new_dict.remove(b"DecodeParms");
            new_dict.remove(b"DL");
            let new_stream = Stream::flate(new_dict, &new_data);
            doc.set(r, Object::Stream(new_stream));
        }
        Ok(())
    }

    /// Rewrites vector operators in a content stream to true K-only / DeviceGray.
    pub fn filter_operators(ops: &[Op]) -> Vec<Op> {
        let mut out: Vec<Op> = Vec::with_capacity(ops.len());

        for op in ops {
            match op.op.as_slice() {
                // RGB non-stroking: r g b rg -> Gray g
                b"rg" => {
                    if let Some([r, g, b]) = op.nums::<3>() {
                        let gray = (0.299 * r + 0.587 * g + 0.114 * b).clamp(0.0, 1.0);
                        out.push(Op::new("g", vec![pdfcraft_content::num(gray)]));
                    } else {
                        out.push(op.clone());
                    }
                }

                // RGB stroking: r g b RG -> Gray G
                b"RG" => {
                    if let Some([r, g, b]) = op.nums::<3>() {
                        let gray = (0.299 * r + 0.587 * g + 0.114 * b).clamp(0.0, 1.0);
                        out.push(Op::new("G", vec![pdfcraft_content::num(gray)]));
                    } else {
                        out.push(op.clone());
                    }
                }

                // CMYK non-stroking: c m y k k -> 0 0 0 K_eff k
                b"k" => {
                    if let Some([c, m, y, k]) = op.nums::<4>() {
                        let luma_cmy = 0.299 * c + 0.587 * m + 0.114 * y;
                        let k_eff = (k + luma_cmy).min(1.0).clamp(0.0, 1.0);
                        out.push(Op::new("k", vec![
                            pdfcraft_content::num(0.0),
                            pdfcraft_content::num(0.0),
                            pdfcraft_content::num(0.0),
                            pdfcraft_content::num(k_eff),
                        ]));
                    } else {
                        out.push(op.clone());
                    }
                }

                // CMYK stroking: c m y k K -> 0 0 0 K_eff K
                b"K" => {
                    if let Some([c, m, y, k]) = op.nums::<4>() {
                        let luma_cmy = 0.299 * c + 0.587 * m + 0.114 * y;
                        let k_eff = (k + luma_cmy).min(1.0).clamp(0.0, 1.0);
                        out.push(Op::new("K", vec![
                            pdfcraft_content::num(0.0),
                            pdfcraft_content::num(0.0),
                            pdfcraft_content::num(0.0),
                            pdfcraft_content::num(k_eff),
                        ]));
                    } else {
                        out.push(op.clone());
                    }
                }

                // ColorSpace selection: /DeviceRGB or /DeviceCMYK -> /DeviceGray
                b"cs" | b"CS" => {
                    let mut new_op = op.clone();
                    if let Some(first) = new_op.operands.first_mut() {
                        Self::rewrite_colorspace_operand(first);
                    }
                    out.push(new_op);
                }

                // Color setting: sc, scn
                b"sc" | b"scn" => {
                    let nums: Vec<f64> = op.operands.iter().filter_map(Object::as_f64).collect();
                    match nums.len() {
                        3 => {
                            let gray = (0.299 * nums[0] + 0.587 * nums[1] + 0.114 * nums[2]).clamp(0.0, 1.0);
                            out.push(Op::new("g", vec![pdfcraft_content::num(gray)]));
                        }
                        4 => {
                            let luma_cmy = 0.299 * nums[0] + 0.587 * nums[1] + 0.114 * nums[2];
                            let k_eff = (nums[3] + luma_cmy).min(1.0).clamp(0.0, 1.0);
                            out.push(Op::new("k", vec![
                                pdfcraft_content::num(0.0),
                                pdfcraft_content::num(0.0),
                                pdfcraft_content::num(0.0),
                                pdfcraft_content::num(k_eff),
                            ]));
                        }
                        _ => out.push(op.clone()),
                    }
                }

                // Color setting: SC, SCN
                b"SC" | b"SCN" => {
                    let nums: Vec<f64> = op.operands.iter().filter_map(Object::as_f64).collect();
                    match nums.len() {
                        3 => {
                            let gray = (0.299 * nums[0] + 0.587 * nums[1] + 0.114 * nums[2]).clamp(0.0, 1.0);
                            out.push(Op::new("G", vec![pdfcraft_content::num(gray)]));
                        }
                        4 => {
                            let luma_cmy = 0.299 * nums[0] + 0.587 * nums[1] + 0.114 * nums[2];
                            let k_eff = (nums[3] + luma_cmy).min(1.0).clamp(0.0, 1.0);
                            out.push(Op::new("K", vec![
                                pdfcraft_content::num(0.0),
                                pdfcraft_content::num(0.0),
                                pdfcraft_content::num(0.0),
                                pdfcraft_content::num(k_eff),
                            ]));
                        }
                        _ => out.push(op.clone()),
                    }
                }

                // Inline images: BI ... ID ... EI
                b"BI" => {
                    if let Some((dict, img)) = &op.inline {
                        let new_inline = Self::filter_inline_image(dict, img);
                        out.push(Op {
                            op: op.op.clone(),
                            operands: op.operands.clone(),
                            inline: Some(new_inline),
                            span: op.span.clone(),
                        });
                    } else {
                        out.push(op.clone());
                    }
                }

                _ => out.push(op.clone()),
            }
        }

        out
    }

    /// Rewrites color space operand name/array to DeviceGray.
    fn rewrite_colorspace_operand(operand: &mut Object) {
        match operand {
            Object::Name(n) => {
                if matches!(n.as_slice(), b"DeviceRGB" | b"RGB" | b"DeviceCMYK" | b"CMYK" | b"CalRGB") {
                    *n = b"DeviceGray".to_vec();
                }
            }
            Object::Array(arr) => {
                if let Some(first) = arr.first_mut() {
                    Self::rewrite_colorspace_operand(first);
                }
            }
            _ => {}
        }
    }

    /// Converts an inline image to DeviceGray with 8-bit luminance samples.
    fn filter_inline_image(dict: &Dict, data: &[u8]) -> (Dict, Vec<u8>) {
        let is_mask = dict.get(b"IM").or_else(|| dict.get(b"ImageMask"))
            .is_some_and(|im| matches!(im, Object::Bool(true)));
        if is_mask {
            return (dict.clone(), data.to_vec());
        }

        let cs_name = dict.get(b"CS").or_else(|| dict.get(b"ColorSpace"))
            .and_then(Object::as_name);
        let is_rgb = matches!(cs_name, Some(b"DeviceRGB" | b"RGB"));
        let is_cmyk = matches!(cs_name, Some(b"DeviceCMYK" | b"CMYK"));

        if !is_rgb && !is_cmyk {
            return (dict.clone(), data.to_vec());
        }

        // Build filter chain from /F or /Filter (supports single name or array of names)
        let filter_obj = dict.get(b"F").or_else(|| dict.get(b"Filter"));
        let mut chain: Vec<(pdfcraft_filters::Filter, pdfcraft_filters::Params)> = Vec::new();
        if let Some(f_obj) = filter_obj {
            match f_obj {
                Object::Name(name) => {
                    let filter = pdfcraft_filters::Filter::from_name(name);
                    chain.push((filter, pdfcraft_filters::Params::default()));
                }
                Object::Array(arr) => {
                    for item in arr {
                        if let Some(name) = item.as_name() {
                            let filter = pdfcraft_filters::Filter::from_name(name);
                            chain.push((filter, pdfcraft_filters::Params::default()));
                        }
                    }
                }
                _ => {}
            }
        }

        // Verify all filters are decodable; preserve untouched if any unsupported image codec
        for (f, _) in &chain {
            if !f.is_decodable() {
                return (dict.clone(), data.to_vec());
            }
        }

        // Decompress samples using pdfcraft_filters
        let raw_samples = if chain.is_empty() {
            data.to_vec()
        } else {
            match pdfcraft_filters::decode(&chain, data, 100 * 1024 * 1024) {
                Ok(decompressed) => decompressed,
                Err(_) => return (dict.clone(), data.to_vec()),
            }
        };

        let decode_inverted: Vec<bool> = if let Some(Object::Array(arr)) = dict.get(b"D").or_else(|| dict.get(b"Decode")) {
            arr.chunks(2)
                .map(|pair| {
                    let d_min = pair.first().and_then(Object::as_f64).unwrap_or(0.0);
                    let d_max = pair.get(1).and_then(Object::as_f64).unwrap_or(1.0);
                    d_min > d_max
                })
                .collect()
        } else {
            Vec::new()
        };
        let is_inverted = |comp_idx: usize| decode_inverted.get(comp_idx).copied().unwrap_or(false);

        let gray_samples = if is_rgb {
            let mut gray = Vec::with_capacity(raw_samples.len() / 3);
            let r_inv = is_inverted(0);
            let g_inv = is_inverted(1);
            let b_inv = is_inverted(2);
            for chunk in raw_samples.chunks_exact(3) {
                let r = if r_inv { 255 - chunk[0] as u32 } else { chunk[0] as u32 };
                let g = if g_inv { 255 - chunk[1] as u32 } else { chunk[1] as u32 };
                let b = if b_inv { 255 - chunk[2] as u32 } else { chunk[2] as u32 };
                let y = ((299 * r + 587 * g + 114 * b + 500) / 1000) as u8;
                gray.push(y);
            }
            gray
        } else {
            // CMYK
            let mut gray = Vec::with_capacity(raw_samples.len() / 4);
            let c_inv = is_inverted(0);
            let m_inv = is_inverted(1);
            let y_inv = is_inverted(2);
            let k_inv = is_inverted(3);
            for chunk in raw_samples.chunks_exact(4) {
                let c = if c_inv { 255 - chunk[0] as u32 } else { chunk[0] as u32 };
                let m = if m_inv { 255 - chunk[1] as u32 } else { chunk[1] as u32 };
                let y_cmy = if y_inv { 255 - chunk[2] as u32 } else { chunk[2] as u32 };
                let k = if k_inv { 255 - chunk[3] as u32 } else { chunk[3] as u32 };
                let l_cmy = (299 * c + 587 * m + 114 * y_cmy + 500) / 1000;
                let k_eff = (k + l_cmy).min(255);
                let gray_val = (255 - k_eff) as u8;
                gray.push(gray_val);
            }
            gray
        };

        if gray_samples.is_empty() && !raw_samples.is_empty() {
            return (dict.clone(), data.to_vec());
        }

        let mut new_dict = dict.clone();

        // Re-encode grayscale samples preserving the original filter encoding
        let mut encoded = gray_samples.clone();
        let mut enc_ok = true;
        for (filter, params) in chain.iter().rev() {
            match pdfcraft_filters::encode(filter, params, &encoded) {
                Ok(res) => encoded = res,
                Err(_) => {
                    enc_ok = false;
                    break;
                }
            }
        }

        let final_data = if enc_ok {
            encoded
        } else {
            // Fallback: encode with Flate if reverse encoding failed
            new_dict.remove(b"Filter");
            new_dict.remove(b"F");
            new_dict.remove(b"DecodeParms");
            new_dict.remove(b"DP");
            new_dict.set(b"F".to_vec(), Object::Name(b"Fl".to_vec()));
            pdfcraft_filters::encode_flate(&gray_samples)
        };

        new_dict.remove(b"ColorSpace");
        new_dict.remove(b"CS");
        new_dict.set(b"CS".to_vec(), Object::Name(b"DeviceGray".to_vec()));

        // Strip multi-component Decode array invalid for 1-component DeviceGray
        if let Some(dec) = new_dict.get(b"D").or_else(|| new_dict.get(b"Decode")) {
            if dec.as_array().is_some_and(|a| a.len() > 2) {
                new_dict.remove(b"D");
                new_dict.remove(b"Decode");
            }
        }

        (new_dict, final_data)
    }

    /// Recursively processes Form XObjects in resources.
    fn filter_resources(
        doc: &mut Document,
        res_obj: &Object,
        visited: &mut HashSet<ObjRef>,
    ) -> Result<(), PrintError> {
        let Some(res_dict) = doc.resolve(res_obj).as_dict().cloned() else {
            return Ok(());
        };
        let Some(xo_dict) = res_dict.get(b"XObject").map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned()) else {
            return Ok(());
        };

        for (_, xo) in xo_dict.iter() {
            let Some(r) = xo.as_ref() else {
                continue;
            };
            if !visited.insert(r) {
                continue;
            }
            let Object::Stream(s) = &*doc.get(r) else {
                continue;
            };
            if s.dict.name(b"Subtype") != Some(b"Form") {
                continue;
            }
            if let Ok(data) = s.decoded() {
                let parsed = parse(&data);
                let filtered = Self::filter_operators(&parsed.ops);
                let new_data = serialize_ops(&filtered);
                let mut new_dict = s.dict.clone();
                new_dict.remove(b"Length");
                new_dict.remove(b"Filter");
                new_dict.remove(b"DecodeParms");
                doc.set(r, Object::Stream(Stream::flate(new_dict, &new_data)));
            }
            if let Some(inner_res) = s.dict.get(b"Resources") {
                Self::filter_resources(doc, inner_res, visited)?;
            }
        }
        Ok(())
    }

    /// Rewrites annotation appearance streams.
    fn filter_annots(
        doc: &mut Document,
        annots_obj: &Object,
        visited: &mut HashSet<ObjRef>,
    ) -> Result<(), PrintError> {
        let Some(annots_arr) = doc.resolve(annots_obj).as_array().cloned() else {
            return Ok(());
        };
        for annot in annots_arr {
            let Some(annot_dict) = doc.resolve(&annot).as_dict().cloned() else {
                continue;
            };
            let Some(ap_obj) = annot_dict.get(b"AP") else {
                continue;
            };
            let Some(ap_dict) = doc.resolve(ap_obj).as_dict().cloned() else {
                continue;
            };

            for (_, stream_or_dict) in ap_dict.iter() {
                let resolved = doc.resolve(stream_or_dict);
                if let Some(sub_dict) = resolved.as_dict() {
                    for (_, state_stream) in sub_dict.iter() {
                        if let Some(r) = state_stream.as_ref().filter(|&r| visited.insert(r)) {
                            Self::filter_form_stream(doc, r)?;
                        }
                    }
                } else if let Some(r) = stream_or_dict.as_ref().filter(|&r| visited.insert(r)) {
                    Self::filter_form_stream(doc, r)?;
                }
            }
        }
        Ok(())
    }

    fn filter_form_stream(doc: &mut Document, r: ObjRef) -> Result<(), PrintError> {
        let Object::Stream(s) = &*doc.get(r) else {
            return Ok(());
        };
        let Ok(data) = s.decoded() else {
            return Ok(());
        };
        let parsed = parse(&data);
        let filtered = Self::filter_operators(&parsed.ops);
        let new_data = serialize_ops(&filtered);
        let mut new_dict = s.dict.clone();
        new_dict.remove(b"Length");
        new_dict.remove(b"Filter");
        new_dict.remove(b"DecodeParms");
        doc.set(r, Object::Stream(Stream::flate(new_dict, &new_data)));
        Ok(())
    }

    fn filter_transparency_group(doc: &mut Document, page_ref: ObjRef, group_obj: &Object) {
        if let Some(r) = group_obj.as_ref() {
            let _ = doc.update_dict(r, |d| {
                if d.contains(b"CS") {
                    d.set(b"CS".to_vec(), Object::Name(b"DeviceGray".to_vec()));
                }
                if d.contains(b"ColorSpace") {
                    d.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceGray".to_vec()));
                }
            });
        } else if let Object::Dict(_) = group_obj {
            let _ = doc.update_dict(page_ref, |page_dict| {
                if let Some(group) = page_dict.get_mut(b"Group") {
                    if let Some(d) = group.as_dict_mut() {
                        if d.contains(b"CS") {
                            d.set(b"CS".to_vec(), Object::Name(b"DeviceGray".to_vec()));
                        }
                        if d.contains(b"ColorSpace") {
                            d.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceGray".to_vec()));
                        }
                    }
                }
            });
        }
    }

    /// Traverses all indirect objects in the document and re-encodes color Image XObjects to 8-bit DeviceGray.
    pub fn filter_image_xobjects(doc: &mut Document) -> Result<(), PrintError> {
        let object_numbers = doc.object_numbers();

        for num in object_numbers {
            let r = ObjRef::new(num, doc.generation(num));
            let obj = doc.get(r);

            let Object::Stream(stream) = &*obj else {
                continue;
            };

            // Verify this is an Image XObject: /Subtype /Image
            if stream.dict.name(b"Subtype") != Some(b"Image") {
                continue;
            }

            // Edge case: ImageMask true (1-bit stencil) must NOT be converted
            if Self::is_image_mask(&stream.dict) {
                continue;
            }

            // Check color space
            let Some(cs_obj) = stream.dict.get(b"ColorSpace").map(|cs| doc.resolve(cs)) else {
                continue;
            };

            // Edge case: Already DeviceGray -> Skip
            if Self::is_device_gray(&cs_obj) {
                continue;
            }

            // Handle Indexed Color Space: [/Indexed base hival lookup]
            if let Some(new_stream) = Self::try_convert_indexed_image(doc, stream, &cs_obj)? {
                doc.set(r, Object::Stream(new_stream));
                continue;
            }

            // Handle Direct RGB / CMYK Images
            if let Some(new_stream) = Self::convert_raster_image_to_gray(doc, stream, &cs_obj)? {
                doc.set(r, Object::Stream(new_stream));
            }
        }

        Ok(())
    }

    fn is_image_mask(dict: &Dict) -> bool {
        dict.get(b"ImageMask")
            .is_some_and(|m| matches!(m, Object::Bool(true)))
    }

    fn is_device_gray(cs: &Object) -> bool {
        match cs {
            Object::Name(n) => n == b"DeviceGray" || n == b"G",
            Object::Array(a) => a.first().and_then(Object::as_name) == Some(b"DeviceGray"),
            _ => false,
        }
    }

    /// Converts an RGB or CMYK raster stream to an 8-bit DeviceGray Flate stream.
    fn convert_raster_image_to_gray(
        doc: &Document,
        stream: &Stream,
        cs: &Object,
    ) -> Result<Option<Stream>, PrintError> {
        let width = stream.dict.int(b"Width").unwrap_or(0) as usize;
        let height = stream.dict.int(b"Height").unwrap_or(0) as usize;
        let bpc = stream.dict.int(b"BitsPerComponent").unwrap_or(8) as usize;

        if width == 0 || height == 0 {
            return Ok(None);
        }

        // Determine number of color components
        let components = match cs {
            Object::Name(n) => match n.as_slice() {
                b"DeviceRGB" | b"RGB" => 3,
                b"DeviceCMYK" | b"CMYK" => 4,
                _ => return Ok(None),
            },
            Object::Array(a) => match a.first().and_then(Object::as_name) {
                Some(b"DeviceRGB") => 3,
                Some(b"DeviceCMYK") => 4,
                Some(b"CalRGB") => 3,
                Some(b"ICCBased") => {
                    let icc_stream = doc.resolve(a.get(1).unwrap_or(&Object::Null));
                    if let Object::Stream(s) = &*icc_stream {
                        s.dict.int(b"N").unwrap_or(3) as usize
                    } else {
                        3
                    }
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };

        if components != 3 && components != 4 {
            return Ok(None);
        }

        // Validate BPC (BitsPerComponent): supported values are 8 and 16
        if bpc != 8 && bpc != 16 {
            return Ok(None);
        }

        // Decompress the stream using cos / filters (unrolls predictors)
        let decoded_samples = match stream.decoded() {
            Ok(data) => data,
            Err(CosError::Filter(f)) => {
                log::warn!("Skipping image with unsupported filter: {f}");
                return Ok(None);
            }
            Err(e) => return Err(PrintError::Invalid(format!("corrupt image stream: {e}"))),
        };

        let total_pixels = width
            .checked_mul(height)
            .ok_or_else(|| PrintError::Invalid("image dimensions overflow".into()))?;

        // Extract Decode array inversion flags per component:
        // In PDF (ISO 32000-1 §8.9.5.2), Decode entry contains pairs [Dmin, Dmax] per component.
        // For standard DeviceRGB/DeviceCMYK, default is [0.0, 1.0, ...].
        // If Dmin > Dmax (e.g. [1.0, 0.0, ...]), the component values are inverted:
        // raw 0 represents 100% ink / saturation and 255 (or 65535) represents 0% ink.
        let decode_inverted: Vec<bool> = if let Some(d_obj) = stream.dict.get(b"Decode").map(|d| doc.resolve(d)) {
            if let Object::Array(arr) = &*d_obj {
                arr.chunks(2)
                    .map(|pair| {
                        let d_min = pair.first().and_then(Object::as_f64).unwrap_or(0.0);
                        let d_max = pair.get(1).and_then(Object::as_f64).unwrap_or(1.0);
                        d_min > d_max
                    })
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let is_inverted = |comp_idx: usize| decode_inverted.get(comp_idx).copied().unwrap_or(false);

        // Allocate 8-bit Gray buffer (1 byte per pixel)
        let mut gray_pixels = Vec::with_capacity(total_pixels);

        match (components, bpc) {
            (3, 8) => {
                // 8-bit RGB -> Gray: Y = (299*R + 587*G + 114*B + 500) / 1000
                let expected = total_pixels.saturating_mul(3);
                if decoded_samples.len() < expected {
                    return Err(PrintError::Invalid("truncated 8-bit RGB image data".into()));
                }
                let r_inv = is_inverted(0);
                let g_inv = is_inverted(1);
                let b_inv = is_inverted(2);

                for chunk in decoded_samples[..expected].chunks_exact(3) {
                    let r = if r_inv { 255 - chunk[0] as u32 } else { chunk[0] as u32 };
                    let g = if g_inv { 255 - chunk[1] as u32 } else { chunk[1] as u32 };
                    let b = if b_inv { 255 - chunk[2] as u32 } else { chunk[2] as u32 };
                    let y = ((299 * r + 587 * g + 114 * b + 500) / 1000) as u8;
                    gray_pixels.push(y);
                }
            }
            (3, 16) => {
                // 16-bit RGB -> Gray:
                // Big-endian 16-bit component pairs (2 bytes per component, 6 bytes per pixel).
                // High byte is at chunk[0], chunk[2], chunk[4].
                let expected = total_pixels.saturating_mul(6);
                if decoded_samples.len() < expected {
                    return Err(PrintError::Invalid("truncated 16-bit RGB image data".into()));
                }
                let r_inv = is_inverted(0);
                let g_inv = is_inverted(1);
                let b_inv = is_inverted(2);

                for chunk in decoded_samples[..expected].chunks_exact(6) {
                    let r_raw = chunk[0] as u32; // Extract high byte (normalized 0..255)
                    let g_raw = chunk[2] as u32;
                    let b_raw = chunk[4] as u32;

                    let r = if r_inv { 255 - r_raw } else { r_raw };
                    let g = if g_inv { 255 - g_raw } else { g_raw };
                    let b = if b_inv { 255 - b_raw } else { b_raw };

                    let y = ((299 * r + 587 * g + 114 * b + 500) / 1000) as u8;
                    gray_pixels.push(y);
                }
            }
            (4, 8) => {
                // 8-bit CMYK -> Gray:
                // K_eff = min(255, K + (299*C + 587*M + 114*Y + 500)/1000); Y = 255 - K_eff
                let expected = total_pixels.saturating_mul(4);
                if decoded_samples.len() < expected {
                    return Err(PrintError::Invalid("truncated 8-bit CMYK image data".into()));
                }
                let c_inv = is_inverted(0);
                let m_inv = is_inverted(1);
                let y_inv = is_inverted(2);
                let k_inv = is_inverted(3);

                for chunk in decoded_samples[..expected].chunks_exact(4) {
                    let c = if c_inv { 255 - chunk[0] as u32 } else { chunk[0] as u32 };
                    let m = if m_inv { 255 - chunk[1] as u32 } else { chunk[1] as u32 };
                    let y_cmy = if y_inv { 255 - chunk[2] as u32 } else { chunk[2] as u32 };
                    let k = if k_inv { 255 - chunk[3] as u32 } else { chunk[3] as u32 };

                    let l_cmy = (299 * c + 587 * m + 114 * y_cmy + 500) / 1000;
                    let k_eff = (k + l_cmy).min(255);
                    let gray = (255 - k_eff) as u8;
                    gray_pixels.push(gray);
                }
            }
            (4, 16) => {
                // 16-bit CMYK -> Gray:
                // Big-endian 16-bit component pairs (2 bytes per component, 8 bytes per pixel).
                // High byte is at chunk[0], chunk[2], chunk[4], chunk[6].
                let expected = total_pixels.saturating_mul(8);
                if decoded_samples.len() < expected {
                    return Err(PrintError::Invalid("truncated 16-bit CMYK image data".into()));
                }
                let c_inv = is_inverted(0);
                let m_inv = is_inverted(1);
                let y_inv = is_inverted(2);
                let k_inv = is_inverted(3);

                for chunk in decoded_samples[..expected].chunks_exact(8) {
                    let c_raw = chunk[0] as u32;
                    let m_raw = chunk[2] as u32;
                    let y_raw = chunk[4] as u32;
                    let k_raw = chunk[6] as u32;

                    let c = if c_inv { 255 - c_raw } else { c_raw };
                    let m = if m_inv { 255 - m_raw } else { m_raw };
                    let y_cmy = if y_inv { 255 - y_raw } else { y_raw };
                    let k = if k_inv { 255 - k_raw } else { k_raw };

                    let l_cmy = (299 * c + 587 * m + 114 * y_cmy + 500) / 1000;
                    let k_eff = (k + l_cmy).min(255);
                    let gray = (255 - k_eff) as u8;
                    gray_pixels.push(gray);
                }
            }
            _ => {
                // Fallback for non-8/16-bit or unhandled variant
                return Ok(None);
            }
        }

        // Construct normalized dictionary
        let mut new_dict = stream.dict.clone();
        new_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceGray".to_vec()));
        new_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));
        new_dict.remove(b"DecodeParms");
        new_dict.remove(b"Decode");
        new_dict.remove(b"Filter");
        new_dict.remove(b"Length");

        // Recompress with Flate (zlib)
        let new_stream = Stream::flate(new_dict, &gray_pixels);
        Ok(Some(new_stream))
    }

    /// Converts an Indexed color space palette to DeviceGray in-place.
    fn try_convert_indexed_image(
        doc: &Document,
        stream: &Stream,
        cs: &Object,
    ) -> Result<Option<Stream>, PrintError> {
        let Object::Array(a) = cs else {
            return Ok(None);
        };
        if a.first().and_then(Object::as_name) != Some(b"Indexed") {
            return Ok(None);
        }

        let Some(base_cs) = a.get(1).map(|b| doc.resolve(b)) else {
            return Ok(None);
        };
        let Some(hival) = a.get(2).and_then(Object::as_int) else {
            return Ok(None);
        };
        let Some(lookup_obj) = a.get(3).map(|l| doc.resolve(l)) else {
            return Ok(None);
        };

        // PDF 1.7 Table 80: hival must be an integer in 0..=255
        if !(0..=255).contains(&hival) {
            return Ok(None);
        }

        // Determine if base color space is RGB or CMYK (handling both Name and Array forms)
        let (is_rgb, is_cmyk) = match &*base_cs {
            Object::Name(n) => match n.as_slice() {
                b"DeviceRGB" | b"RGB" => (true, false),
                b"DeviceCMYK" | b"CMYK" => (false, true),
                b"DeviceGray" | b"G" => return Ok(None), // Already monochrome (idempotence)
                _ => (false, false),
            },
            Object::Array(arr) => match arr.first().and_then(Object::as_name) {
                Some(b"DeviceRGB" | b"CalRGB") => (true, false),
                Some(b"DeviceCMYK") => (false, true),
                Some(b"DeviceGray" | b"CalGray") => return Ok(None), // Already monochrome
                Some(b"ICCBased") => {
                    let icc_stream = doc.resolve(arr.get(1).unwrap_or(&Object::Null));
                    if let Object::Stream(s) = &*icc_stream {
                        let n = s.dict.int(b"N").unwrap_or(3);
                        if n == 4 {
                            (false, true)
                        } else if n == 1 {
                            return Ok(None);
                        } else {
                            (true, false)
                        }
                    } else {
                        (true, false)
                    }
                }
                _ => (false, false),
            },
            _ => (false, false),
        };

        if !is_rgb && !is_cmyk {
            return Ok(None);
        }

        // Extract lookup bytes (handles both literal/hex byte strings and compressed streams)
        let lookup_bytes = match &*lookup_obj {
            Object::String(s) => s.bytes.clone(),
            Object::Stream(s) => s.decoded().unwrap_or_default(),
            _ => return Ok(None),
        };

        let entries = (hival + 1) as usize;
        let mut gray_lookup = Vec::with_capacity(entries);

        if is_rgb {
            if lookup_bytes.len() < entries * 3 {
                return Ok(None);
            }
            // Convert each RGB palette entry (R, G, B) -> DeviceGray Y
            for chunk in lookup_bytes[..entries * 3].chunks_exact(3) {
                let r = chunk[0] as u32;
                let g = chunk[1] as u32;
                let b = chunk[2] as u32;
                let y = ((299 * r + 587 * g + 114 * b + 500) / 1000) as u8;
                gray_lookup.push(y);
            }
        } else if is_cmyk {
            if lookup_bytes.len() < entries * 4 {
                return Ok(None);
            }
            // Convert each CMYK palette entry (C, M, Y, K) -> DeviceGray
            for chunk in lookup_bytes[..entries * 4].chunks_exact(4) {
                let c = chunk[0] as u32;
                let m = chunk[1] as u32;
                let y_cmy = chunk[2] as u32;
                let k = chunk[3] as u32;
                let luma_cmy = (299 * c + 587 * m + 114 * y_cmy + 500) / 1000;
                let k_eff = (k + luma_cmy).min(255);
                let gray_val = (255 - k_eff) as u8;
                gray_lookup.push(gray_val);
            }
        }

        // Build new ColorSpace array: [/Indexed /DeviceGray hival gray_lookup]
        let new_cs = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            Object::Name(b"DeviceGray".to_vec()),
            Object::Int(hival),
            Object::String(PdfString::literal(gray_lookup)),
        ]);

        let mut new_dict = stream.dict.clone();
        new_dict.remove(b"CS");
        new_dict.set(b"ColorSpace".to_vec(), new_cs);

        // Keep stream data intact as pixel indices don't change
        Ok(Some(Stream::from_raw(new_dict, stream.raw.to_vec())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vector_rgb_to_gray() {
        let input_ops = vec![
            Op::new("rg", vec![pdfcraft_content::num(1.0), pdfcraft_content::num(0.0), pdfcraft_content::num(0.0)]),
            Op::new("RG", vec![pdfcraft_content::num(0.0), pdfcraft_content::num(1.0), pdfcraft_content::num(0.0)]),
            Op::new("rg", vec![pdfcraft_content::num(0.0), pdfcraft_content::num(0.0), pdfcraft_content::num(1.0)]),
        ];

        let filtered = MonochromeVectorFilter::filter_operators(&input_ops);
        assert_eq!(filtered.len(), 3);

        // Pure Red -> 0.299 g
        assert_eq!(filtered[0].op, b"g");
        let g0 = filtered[0].num(0).unwrap();
        assert!((g0 - 0.299).abs() < 1e-4);

        // Pure Green -> 0.587 G
        assert_eq!(filtered[1].op, b"G");
        let g1 = filtered[1].num(0).unwrap();
        assert!((g1 - 0.587).abs() < 1e-4);

        // Pure Blue -> 0.114 g
        assert_eq!(filtered[2].op, b"g");
        let g2 = filtered[2].num(0).unwrap();
        assert!((g2 - 0.114).abs() < 1e-4);
    }

    #[test]
    fn test_vector_cmyk_strict_zero_cmy() {
        // c=0.5, m=0.2, y=0.1, k=0.3
        // luma_cmy = 0.299*0.5 + 0.587*0.2 + 0.114*0.1 = 0.1495 + 0.1174 + 0.0114 = 0.2783
        // k_eff = 0.3 + 0.2783 = 0.5783
        let input_ops = vec![
            Op::new("k", vec![
                pdfcraft_content::num(0.5),
                pdfcraft_content::num(0.2),
                pdfcraft_content::num(0.1),
                pdfcraft_content::num(0.3),
            ]),
        ];

        let filtered = MonochromeVectorFilter::filter_operators(&input_ops);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].op, b"k");

        let nums = filtered[0].nums::<4>().unwrap();
        assert_eq!(nums[0], 0.0); // Strict 0 Cyan
        assert_eq!(nums[1], 0.0); // Strict 0 Magenta
        assert_eq!(nums[2], 0.0); // Strict 0 Yellow
        assert!((nums[3] - 0.5783).abs() < 1e-4); // K_eff
    }

    #[test]
    fn test_colorspace_and_sc_operators() {
        let input_ops = vec![
            Op::new("cs", vec![Object::Name(b"DeviceRGB".to_vec())]),
            Op::new("sc", vec![pdfcraft_content::num(1.0), pdfcraft_content::num(0.0), pdfcraft_content::num(0.0)]),
            Op::new("CS", vec![Object::Name(b"DeviceCMYK".to_vec())]),
            Op::new("SC", vec![
                pdfcraft_content::num(0.0),
                pdfcraft_content::num(0.0),
                pdfcraft_content::num(0.0),
                pdfcraft_content::num(0.5),
            ]),
        ];

        let filtered = MonochromeVectorFilter::filter_operators(&input_ops);
        assert_eq!(filtered.len(), 4);

        // cs -> /DeviceGray
        assert_eq!(filtered[0].operands[0], Object::Name(b"DeviceGray".to_vec()));

        // sc 1 0 0 -> g 0.299
        assert_eq!(filtered[1].op, b"g");
        assert!((filtered[1].num(0).unwrap() - 0.299).abs() < 1e-4);

        // CS -> /DeviceGray
        assert_eq!(filtered[2].operands[0], Object::Name(b"DeviceGray".to_vec()));

        // SC 0 0 0 0.5 -> K 0 0 0 0.5
        assert_eq!(filtered[3].op, b"K");
        let k_nums = filtered[3].nums::<4>().unwrap();
        assert_eq!(k_nums[0], 0.0);
        assert_eq!(k_nums[1], 0.0);
        assert_eq!(k_nums[2], 0.0);
        assert_eq!(k_nums[3], 0.5);
    }

    #[test]
    fn test_reencode_rgb_image_to_devicegray() {
        let mut doc = Document::new_empty();

        let rgb_data = vec![
            255, 0, 0,   // Red -> Y = 76
            0, 255, 0,   // Green -> Y = 150
            0, 0, 255,   // Blue -> Y = 29
            255, 255, 255, // White -> Y = 255
        ];

        let mut img_dict = Dict::new();
        img_dict.set(b"Type".to_vec(), Object::Name(b"XObject".to_vec()));
        img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
        img_dict.set(b"Width".to_vec(), Object::Int(2));
        img_dict.set(b"Height".to_vec(), Object::Int(2));
        img_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceRGB".to_vec()));
        img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));

        let stream = Stream::flate(img_dict, &rgb_data);
        let img_ref = doc.add(Object::Stream(stream));

        MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

        let updated = doc.get(img_ref);
        let Object::Stream(s) = &*updated else { panic!("expected stream") };

        assert_eq!(s.dict.name(b"ColorSpace"), Some(b"DeviceGray".as_slice()));
        assert_eq!(s.dict.int(b"BitsPerComponent"), Some(8));
        assert!(s.dict.get(b"DecodeParms").is_none());
        assert_eq!(s.dict.name(b"Filter"), Some(b"FlateDecode".as_slice()));

        let decoded = s.decoded().unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded, vec![76, 150, 29, 255]);
    }

    #[test]
    fn test_reencode_cmyk_image_to_devicegray() {
        let mut doc = Document::new_empty();

        let cmyk_data = vec![
            0, 0, 0, 255,   // Pure Black -> Y = 0
            0, 0, 0, 0,     // Pure White -> Y = 255
            0, 0, 0, 128,   // 50% Black -> Y = 127
            255, 0, 0, 0,   // Pure Cyan (L_cmy = 76) -> Y = 179
        ];

        let mut img_dict = Dict::new();
        img_dict.set(b"Type".to_vec(), Object::Name(b"XObject".to_vec()));
        img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
        img_dict.set(b"Width".to_vec(), Object::Int(2));
        img_dict.set(b"Height".to_vec(), Object::Int(2));
        img_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceCMYK".to_vec()));
        img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));

        let stream = Stream::flate(img_dict, &cmyk_data);
        let img_ref = doc.add(Object::Stream(stream));

        MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

        let updated = doc.get(img_ref);
        let Object::Stream(s) = &*updated else { panic!("expected stream") };

        assert_eq!(s.dict.name(b"ColorSpace"), Some(b"DeviceGray".as_slice()));
        let decoded = s.decoded().unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded, vec![0, 255, 127, 179]);
    }

    #[test]
    fn test_bypass_already_grayscale_and_imagemask() {
        let mut doc = Document::new_empty();

        // DeviceGray Image
        let mut gray_dict = Dict::new();
        gray_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
        gray_dict.set(b"Width".to_vec(), Object::Int(1));
        gray_dict.set(b"Height".to_vec(), Object::Int(1));
        gray_dict.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceGray".to_vec()));
        gray_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));
        let gray_ref = doc.add(Object::Stream(Stream::flate(gray_dict, &[128])));

        // ImageMask 1-bit Stencil
        let mut mask_dict = Dict::new();
        mask_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
        mask_dict.set(b"Width".to_vec(), Object::Int(8));
        mask_dict.set(b"Height".to_vec(), Object::Int(1));
        mask_dict.set(b"ImageMask".to_vec(), Object::Bool(true));
        mask_dict.set(b"BitsPerComponent".to_vec(), Object::Int(1));
        let mask_ref = doc.add(Object::Stream(Stream::flate(mask_dict, &[0xAA])));

        MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

        let updated_gray = doc.get(gray_ref);
        let Object::Stream(s_gray) = &*updated_gray else { panic!("expected stream") };
        assert_eq!(s_gray.dict.name(b"ColorSpace"), Some(b"DeviceGray".as_slice()));
        assert_eq!(s_gray.decoded().unwrap(), vec![128]);

        let updated_mask = doc.get(mask_ref);
        let Object::Stream(s_mask) = &*updated_mask else { panic!("expected stream") };
        assert_eq!(s_mask.dict.get(b"ImageMask"), Some(&Object::Bool(true)));
    }

    #[test]
    fn test_indexed_colorspace_conversion() {
        let mut doc = Document::new_empty();

        let palette = vec![255, 0, 0, 0, 0, 255];
        let cs = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            Object::Name(b"DeviceRGB".to_vec()),
            Object::Int(1),
            Object::String(pdfcraft_cos::PdfString::literal(palette)),
        ]);

        let mut img_dict = Dict::new();
        img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
        img_dict.set(b"Width".to_vec(), Object::Int(2));
        img_dict.set(b"Height".to_vec(), Object::Int(1));
        img_dict.set(b"ColorSpace".to_vec(), cs);
        img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));

        let stream = Stream::flate(img_dict, &[0, 1]);
        let img_ref = doc.add(Object::Stream(stream));

        MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

        let updated = doc.get(img_ref);
        let Object::Stream(s) = &*updated else { panic!("expected stream") };

        let cs = s.dict.get(b"ColorSpace").unwrap();
        let Object::Array(a) = cs else { panic!("expected array") };
        assert_eq!(a[0], Object::Name(b"Indexed".to_vec()));
        assert_eq!(a[1], Object::Name(b"DeviceGray".to_vec()));
        assert_eq!(a[2], Object::Int(1));
        assert_eq!(a[3], Object::String(pdfcraft_cos::PdfString::literal(vec![76, 29])));
    }

    #[test]
    fn test_indexed_cmyk_colorspace_conversion() {
        let mut doc = Document::new_empty();

        // 2 entries: Entry 0 = Cyan (255, 0, 0, 0) -> 179, Entry 1 = Magenta (0, 255, 0, 0) -> 105
        let palette = vec![
            255, 0, 0, 0,
            0, 255, 0, 0,
        ];
        let cs = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            Object::Name(b"DeviceCMYK".to_vec()),
            Object::Int(1),
            Object::String(pdfcraft_cos::PdfString::literal(palette)),
        ]);

        let mut img_dict = Dict::new();
        img_dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
        img_dict.set(b"Width".to_vec(), Object::Int(2));
        img_dict.set(b"Height".to_vec(), Object::Int(1));
        img_dict.set(b"ColorSpace".to_vec(), cs);
        img_dict.set(b"BitsPerComponent".to_vec(), Object::Int(8));

        let stream = Stream::flate(img_dict, &[0, 1]);
        let img_ref = doc.add(Object::Stream(stream));

        MonochromeVectorFilter::filter_image_xobjects(&mut doc).unwrap();

        let updated = doc.get(img_ref);
        let Object::Stream(s) = &*updated else { panic!("expected stream") };

        let cs = s.dict.get(b"ColorSpace").unwrap();
        let Object::Array(a) = cs else { panic!("expected array") };
        assert_eq!(a[0], Object::Name(b"Indexed".to_vec()));
        assert_eq!(a[1], Object::Name(b"DeviceGray".to_vec()));
        assert_eq!(a[2], Object::Int(1));
        assert_eq!(a[3], Object::String(pdfcraft_cos::PdfString::literal(vec![179, 105])));
    }

    #[test]
    fn test_direct_and_indirect_transparency_group_conversion() {
        let mut doc = Document::new_empty();

        // 1. Indirect transparency group
        let mut indirect_group = Dict::new();
        indirect_group.set(b"Type".to_vec(), Object::Name(b"Group".to_vec()));
        indirect_group.set(b"S".to_vec(), Object::Name(b"Transparency".to_vec()));
        indirect_group.set(b"CS".to_vec(), Object::Name(b"DeviceRGB".to_vec()));
        let indirect_group_ref = doc.add(Object::Dict(indirect_group));

        let page1_ref = ObjRef::new(10, 0);
        MonochromeVectorFilter::filter_transparency_group(
            &mut doc,
            page1_ref,
            &Object::Ref(indirect_group_ref),
        );

        let updated_indirect = doc.get(indirect_group_ref);
        let Object::Dict(d_ind) = &*updated_indirect else { panic!("expected dict") };
        assert_eq!(d_ind.name(b"CS"), Some(b"DeviceGray".as_slice()));

        // 2. Direct inline transparency group dictionary
        let mut direct_group = Dict::new();
        direct_group.set(b"Type".to_vec(), Object::Name(b"Group".to_vec()));
        direct_group.set(b"S".to_vec(), Object::Name(b"Transparency".to_vec()));
        direct_group.set(b"CS".to_vec(), Object::Name(b"DeviceRGB".to_vec()));

        let mut page2_dict = Dict::new();
        page2_dict.set(b"Type".to_vec(), Object::Name(b"Page".to_vec()));
        page2_dict.set(b"Group".to_vec(), Object::Dict(direct_group.clone()));
        let page2_ref = doc.add(Object::Dict(page2_dict));

        let group_obj = Object::Dict(direct_group);
        MonochromeVectorFilter::filter_transparency_group(
            &mut doc,
            page2_ref,
            &group_obj,
        );

        let updated_page = doc.get(page2_ref);
        let Object::Dict(d_page) = &*updated_page else { panic!("expected dict") };
        let group_val = d_page.get(b"Group").expect("group must exist");
        let Object::Dict(d_grp) = group_val else { panic!("expected inline dict") };
        assert_eq!(d_grp.name(b"CS"), Some(b"DeviceGray".as_slice()));
    }
}
