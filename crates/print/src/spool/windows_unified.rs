//! Unified Windows PrintTicket & DEVMODEW broadcast specification normalization (Layer 4).
//!
//! Ensures explicit Color vs. Monochrome (`psk:Color` / vendor color option vs. `psk:Monochrome` /
//! `psk:Grayscale`), `1.0 mm` paper matching with `psk:CustomMediaSize` and safe `ShrinkToFit`
//! adaptation when fixed-tray drivers substitute paper, automatic Simplex fallback on simplex-only
//! hardware, and immediate release of `PRINT_TICKET_PROVIDER` at `progress(0, total)` so multiple
//! printers spool pages concurrently.

#![cfg_attr(not(windows), allow(dead_code))]

use std::sync::Arc;

use super::{
    CapturedDriverTemplate, Duplex, Job, OPT_COLOR_INTENT, OPT_ORIENTATION, OPT_PAPER_UM,
};
use crate::{
    Content, Layout, Orientation, PAPERS, PrintError, Settings, SizeMode, impose, matching_paper,
};

pub(crate) const NS_PSF: &str =
    "http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework";
pub(crate) const NS_PSK: &str =
    "http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords";
const NS_XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
const NS_XSD: &str = "http://www.w3.org/2001/XMLSchema";
const NS_XML: &str = "http://www.w3.org/XML/1998/namespace";

/// Practical fleet-wide paper-matching tolerance (`1.0 mm` = `1000 µm`).
pub const PAPER_MATCH_TOLERANCE_UM: u32 = 1_000;
/// Tolerance within which a validated driver ticket is accepted as 1:1 exact paper (`1.5 mm`).
const RETAINED_PAPER_TOLERANCE_UM: u32 = 1_500;
/// Maximum valid physical side (`200 inches` = `5,080,000 µm`).
const MAX_PAPER_UM: u32 = 5_080_000;
/// Minimum sheet tolerance in points when comparing portrait vs landscape (`0.5 pt`).
const SHEET_SAME_PT: f64 = 0.5;

/// Normalized Windows `DEVMODEW` + base `PrintTicket` seed supplied by `pdfcraft-platform` (`L7`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsNormalizedSeed {
    /// Base `PrintTicket` XML produced via `PTConvertDevModeToPrintTicket` from a validated `DEVMODEW`.
    pub base_ticket_xml: Option<Vec<u8>>,
    /// Driver-validated `DEVMODEW` bytes (`DocumentPropertiesW(DM_IN_BUFFER | DM_OUT_BUFFER)`).
    pub devmode_bytes: Vec<u8>,
    /// Effective duplex mode after checking physical printer duplex support.
    pub effective_duplex: Option<Duplex>,
    /// Whether the target printer physically supports automatic two-sided printing.
    pub supports_duplex: Option<bool>,
    /// Whether the target printer physically supports color output.
    pub supports_color: Option<bool>,
    /// Physical paper dimensions `(width_um, height_um)` retained by the driver's `DEVMODEW`.
    pub retained_paper_um: Option<(u32, u32)>,
    /// True when the driver rejected custom paper dimensions so sheets must safely `ShrinkToFit`.
    pub adapted_shrink_to_fit: bool,
}

/// Requested output color policy extracted from a [`Job`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorIntent {
    /// Explicitly request color (`psk:Color` / `DMCOLOR_COLOR = 2`), overriding B&W queue defaults.
    ExplicitColor,
    /// Explicitly request monochrome (`psk:Monochrome` / `Grayscale` + `DMCOLOR_MONOCHROME = 1`).
    ExplicitMonochrome,
    /// Legacy fallback when no explicit color broadcast intent was stamped on the job.
    DriverDefault,
}

/// Summary of the print-ready PDF's sheet dimensions and broadcast intent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JobSheetTarget {
    pub sheet_count: usize,
    pub first_sheet_pt: (f64, f64),
    pub target_paper_um: (u32, u32),
    pub landscape: bool,
    pub color_intent: ColorIntent,
}

/// Stamp unified broadcast specifications onto `job` idempotently.
pub fn apply_broadcast_specs(
    job: &mut Job,
    explicit_color: bool,
    paper_um: (u32, u32),
    landscape: Option<bool>,
) {
    job.options.retain(|(k, _)| {
        !matches!(k.as_str(), OPT_COLOR_INTENT | OPT_PAPER_UM | OPT_ORIENTATION)
    });
    job.grayscale = !explicit_color;
    let color_val = if explicit_color { "color" } else { "monochrome" };
    job.options.push((OPT_COLOR_INTENT.to_string(), color_val.to_string()));
    if paper_um.0 > 0 && paper_um.1 > 0 {
        job.options
            .push((OPT_PAPER_UM.to_string(), format!("{}x{}", paper_um.0, paper_um.1)));
    }
    let orient_val = match landscape {
        Some(true) => "landscape",
        Some(false) => "portrait",
        None => "auto",
    };
    job.options.push((OPT_ORIENTATION.to_string(), orient_val.to_string()));
}

/// Strip internal `pdfcraft:*` options so non-Windows CUPS `lp` commands never see them.
pub(crate) fn strip_internal_options(job: &Job) -> Job {
    let mut cleaned = job.clone();
    cleaned.options.retain(|(k, _)| !k.starts_with("pdfcraft:"));
    cleaned
}

/// Case-insensitive detection of interactive virtual save-to-file / fax queues.
pub fn is_virtual_save_queue(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    lower.contains("microsoft print to pdf")
        || lower.contains("microsoft xps document writer")
        || lower.contains("xps document writer")
        || lower.contains("onenote")
        || lower == "fax"
        || lower.starts_with("fax ")
        || lower.starts_with("fax-")
        || lower.ends_with(" fax")
        || lower.contains("shared fax")
        || lower.contains("microsoft fax")
}

/// Extract [`ColorIntent`] from `job.options` and `job.grayscale`.
pub fn job_color_intent(job: &Job) -> ColorIntent {
    let tagged = job
        .options
        .iter()
        .rev()
        .find(|(k, _)| k == OPT_COLOR_INTENT)
        .map(|(_, v)| v.trim());
    match tagged {
        Some(v) if v.eq_ignore_ascii_case("color") => ColorIntent::ExplicitColor,
        Some(v) if v.eq_ignore_ascii_case("monochrome") => ColorIntent::ExplicitMonochrome,
        _ if job.grayscale => ColorIntent::ExplicitMonochrome,
        _ => ColorIntent::DriverDefault,
    }
}

/// Parse `OPT_PAPER_UM` (`"{w_um}x{h_um}"`) from `job.options`.
pub fn job_paper_um(job: &Job) -> Option<(u32, u32)> {
    let raw = job
        .options
        .iter()
        .rev()
        .find(|(k, _)| k == OPT_PAPER_UM)
        .map(|(_, v)| v.trim())?;
    let (w_str, h_str) = raw.split_once(['x', 'X'])?;
    let w = w_str.trim().parse::<u32>().ok()?;
    let h = h_str.trim().parse::<u32>().ok()?;
    (w > 0 && h > 0 && w <= MAX_PAPER_UM && h <= MAX_PAPER_UM).then_some((w, h))
}

/// Parse `OPT_ORIENTATION` (`"portrait"` -> `Some(false)`, `"landscape"` -> `Some(true)`).
pub fn job_explicit_landscape(job: &Job) -> Option<bool> {
    let raw = job
        .options
        .iter()
        .rev()
        .find(|(k, _)| k == OPT_ORIENTATION)
        .map(|(_, v)| v.trim())?;
    if raw.eq_ignore_ascii_case("landscape") {
        Some(true)
    } else if raw.eq_ignore_ascii_case("portrait") {
        Some(false)
    } else {
        None
    }
}

/// Convert points (`1/72 in`) to micrometres (`µm`).
pub fn pts_to_um(pts: f64) -> Option<u32> {
    (pts.is_finite() && pts > 0.0 && pts <= 14_400.0)
        .then(|| (pts * 25_400.0 / 72.0).round() as u32)
}

/// Convert micrometres (`µm`) to points (`1/72 in`).
pub fn um_to_pts(um: u32) -> f64 {
    f64::from(um) * 72.0 / 25_400.0
}

/// Inspect the print-ready PDF's sheet geometry and resolve the job's unified target parameters.
pub fn inspect_job_target(pdf_bytes: &[u8], job: &Job) -> Result<JobSheetTarget, PrintError> {
    let doc = pdfcraft_cos::Document::open(Arc::new(pdf_bytes.to_vec()))
        .map_err(|e| PrintError::Spool(format!("the print-ready PDF could not be read: {e}")))?;
    let pages = pdfcraft_model::pages(&doc);
    let first = pages
        .first()
        .map(|p| p.display_size(&doc))
        .ok_or_else(|| PrintError::Spool("the print-ready PDF has no pages".into()))?;
    let sheet_um = pts_to_um(first.0)
        .zip(pts_to_um(first.1))
        .ok_or_else(|| PrintError::Spool("the print-ready PDF has an invalid page size".into()))?;
    let target_paper_um = job_paper_um(job).unwrap_or(sheet_um);
    let sheet_landscape = first.0 > first.1 + SHEET_SAME_PT;
    let landscape = match job_explicit_landscape(job) {
        Some(explicit) if (first.0 - first.1).abs() <= SHEET_SAME_PT => explicit,
        Some(_) | None => sheet_landscape,
    };
    Ok(JobSheetTarget {
        sheet_count: pages.len(),
        first_sheet_pt: first,
        target_paper_um,
        landscape,
        color_intent: job_color_intent(job),
    })
}

/// Extract normalized `CapturedDriverTemplate` fields from a raw Win32 `DEVMODEW` buffer.
pub fn parse_devmode_template_bytes(
    bytes: &[u8],
    fallback_paper_pts: Option<(f64, f64)>,
    known_papers: &[(String, (f64, f64))],
) -> Option<CapturedDriverTemplate> {
    let dm_size = read_u16_le(bytes, 68)? as usize;
    let dm_extra = read_u16_le(bytes, 70)? as usize;
    if dm_size < 156 || dm_size.saturating_add(dm_extra) > bytes.len() {
        return None;
    }
    let fields = read_u32_le(bytes, 72)?;
    let landscape = ((fields & 0x0001) != 0)
        .then(|| read_i16_le(bytes, 76))
        .flatten()
        .and_then(|v| match v {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        });
    let copies = ((fields & 0x0100) != 0)
        .then(|| read_i16_le(bytes, 86))
        .flatten()
        .filter(|&c| (1..=999).contains(&c))
        .map(|c| c as u32);
    let grayscale = ((fields & 0x0800) != 0)
        .then(|| read_i16_le(bytes, 92))
        .flatten()
        .and_then(|v| match v {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        });
    let duplex = ((fields & 0x1000) != 0)
        .then(|| read_i16_le(bytes, 94))
        .flatten()
        .and_then(|v| match v {
            1 => Some(Duplex::Off),
            2 => Some(Duplex::LongEdge),
            3 => Some(Duplex::ShortEdge),
            _ => None,
        });
    let collate = ((fields & 0x8000) != 0)
        .then(|| read_i16_le(bytes, 100))
        .flatten()
        .and_then(|v| match v {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        });
    let dm_paper_um = ((fields & 0x000C) == 0x000C)
        .then(|| read_i16_le(bytes, 82).zip(read_i16_le(bytes, 80)))
        .flatten()
        .filter(|&(w, h)| w > 0 && h > 0)
        .map(|(w, h)| (w as u32 * 100, h as u32 * 100));
    let paper_um = fallback_paper_pts
        .and_then(|(w, h)| pts_to_um(w).zip(pts_to_um(h)))
        .or(dm_paper_um)
        .map(|(w, h)| (w.min(h), w.max(h)));
    let paper_name = paper_um.and_then(|(w, h)| {
        let pts = (um_to_pts(w), um_to_pts(h));
        resolve_template_paper_name(known_papers, pts)
    });
    Some(CapturedDriverTemplate {
        paper_um,
        paper_name,
        grayscale,
        duplex,
        landscape,
        copies,
        collate,
    })
}

fn resolve_template_paper_name(
    known_papers: &[(String, (f64, f64))],
    pts: (f64, f64),
) -> Option<String> {
    let same = |(a, b): (f64, f64), (c, d): (f64, f64)| {
        ((a - c).abs() <= 1.5 && (b - d).abs() <= 1.5)
            || ((a - d).abs() <= 1.5 && (b - c).abs() <= 1.5)
    };
    known_papers
        .iter()
        .find(|(_, p)| same(*p, pts))
        .map(|(n, _)| n.clone())
        .or_else(|| {
            matching_paper(pts).and_then(|idx| PAPERS.get(idx).map(|(n, _)| (*n).to_string()))
        })
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_i16_le(bytes: &[u8], offset: usize) -> Option<i16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(i16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Candidate driver paper entry in micrometres.
#[derive(Debug, Clone)]
pub(crate) struct DriverPaperCandidate<'a> {
    pub width_um: u32,
    pub height_um: u32,
    pub display_name: Option<&'a str>,
    pub is_psk_keyword: bool,
}

/// Match `(width_um, height_um)` (orientation-independent) within `PAPER_MATCH_TOLERANCE_UM` (`1.0 mm`).
pub(crate) fn match_driver_paper(
    papers: &[DriverPaperCandidate<'_>],
    target_um: (u32, u32),
) -> Option<usize> {
    let (w, h) = (target_um.0.min(target_um.1), target_um.0.max(target_um.1));
    let pts = (um_to_pts(w), um_to_pts(h));
    let std_name = matching_paper(pts).and_then(|idx| PAPERS.get(idx).map(|(n, _)| *n));
    let mut matches: Vec<(usize, bool, bool, u32)> = papers
        .iter()
        .enumerate()
        .filter_map(|(idx, p)| {
            let (pw, ph) = (p.width_um.min(p.height_um), p.width_um.max(p.height_um));
            let max_delta = pw.abs_diff(w).max(ph.abs_diff(h));
            if max_delta > PAPER_MATCH_TOLERANCE_UM {
                return None;
            }
            let name_hit = std_name.is_some_and(|sn| {
                p.display_name
                    .is_some_and(|dn| dn.to_ascii_lowercase().contains(&sn.to_ascii_lowercase()))
            });
            Some((idx, !name_hit, !p.is_psk_keyword, pw.abs_diff(w) + ph.abs_diff(h)))
        })
        .collect();
    matches.sort_by_key(|&(_, no_name, no_psk, dist)| (no_name, no_psk, dist));
    matches.first().map(|&(idx, _, _, _)| idx)
}

fn node_is_psk_named(node: roxmltree::Node<'_, '_>, local: &str) -> bool {
    node.attribute("name").is_some_and(|v| {
        let (prefix, name) = v.split_once(':').map_or((None, v), |(p, n)| (Some(p), n));
        name == local && node.lookup_namespace_uri(prefix) == Some(NS_PSK)
    })
}

/// Inspect `PrintCapabilities` XML to determine whether standard duplex bindings are supported.
pub(crate) fn capabilities_support_duplex(capabilities_xml: &str) -> Option<bool> {
    let doc = roxmltree::Document::parse(capabilities_xml).ok()?;
    let root = doc.root_element();
    if !root.has_tag_name((NS_PSF, "PrintCapabilities")) {
        return None;
    }
    let has_two_sided = root
        .children()
        .filter(|n| {
            n.has_tag_name((NS_PSF, "Feature"))
                && (node_is_psk_named(*n, "JobDuplexAllDocumentsContiguously")
                    || node_is_psk_named(*n, "DocumentDuplex"))
        })
        .flat_map(|f| f.children().filter(|n| n.has_tag_name((NS_PSF, "Option"))))
        .any(|o| {
            node_is_psk_named(o, "TwoSidedLongEdge") || node_is_psk_named(o, "TwoSidedShortEdge")
        });
    Some(has_two_sided)
}

/// Determine whether `DocumentDuplex` must be used instead of `JobDuplexAllDocumentsContiguously`.
pub(crate) fn prefers_document_duplex(capabilities_xml: &str, duplex: Option<Duplex>) -> bool {
    let wanted = match duplex {
        None => return false,
        Some(Duplex::Off) => "OneSided",
        Some(Duplex::LongEdge) => "TwoSidedLongEdge",
        Some(Duplex::ShortEdge) => "TwoSidedShortEdge",
    };
    let Ok(doc) = roxmltree::Document::parse(capabilities_xml) else {
        return false;
    };
    let has_opt = |feat: &str| {
        doc.root_element()
            .children()
            .filter(|n| n.has_tag_name((NS_PSF, "Feature")) && node_is_psk_named(*n, feat))
            .flat_map(|f| f.children())
            .any(|o| o.has_tag_name((NS_PSF, "Option")) && node_is_psk_named(o, wanted))
    };
    !has_opt("JobDuplexAllDocumentsContiguously") && has_opt("DocumentDuplex")
}

/// Resolve effective duplex for a printer, degrading `LongEdge`/`ShortEdge` to `Duplex::Off`
/// when hardware or `PrintCapabilities` reports simplex-only support.
pub(crate) fn negotiate_duplex(
    requested: Option<Duplex>,
    seed_supports_duplex: Option<bool>,
    caps_supports_duplex: Option<bool>,
) -> Option<Duplex> {
    match requested {
        Some(Duplex::LongEdge | Duplex::ShortEdge)
            if seed_supports_duplex == Some(false) || caps_supports_duplex == Some(false) =>
        {
            Some(Duplex::Off)
        }
        other => other,
    }
}

/// Select the `PageOutputColor` option QName (and optional vendor namespace declaration)
/// from `PrintCapabilities` XML according to `intent`.
pub(crate) fn select_color_option_xml(
    capabilities_xml: &str,
    intent: ColorIntent,
    seed_supports_color: Option<bool>,
) -> Option<(String, Option<(String, String)>)> {
    if intent == ColorIntent::DriverDefault {
        return None;
    }
    let parsed = roxmltree::Document::parse(capabilities_xml).ok();
    let options: Vec<(String, Option<String>, Option<String>)> = parsed
        .as_ref()
        .map(|doc| {
            doc.root_element()
                .children()
                .filter(|n| {
                    n.has_tag_name((NS_PSF, "Feature")) && node_is_psk_named(*n, "PageOutputColor")
                })
                .flat_map(|f| f.children().filter(|n| n.has_tag_name((NS_PSF, "Option"))))
                .filter_map(|o| {
                    let raw = o.attribute("name")?;
                    let (pref, local) = raw
                        .split_once(':')
                        .map_or((None, raw), |(p, l)| (Some(p.to_string()), l));
                    let uri = pref
                        .as_deref()
                        .and_then(|p| o.lookup_namespace_uri(Some(p)).map(str::to_string));
                    Some((local.to_string(), pref, uri))
                })
                .collect()
        })
        .unwrap_or_default();

    match intent {
        ColorIntent::DriverDefault => None,
        ColorIntent::ExplicitColor => choose_explicit_color(&options, seed_supports_color),
        ColorIntent::ExplicitMonochrome => choose_explicit_monochrome(&options),
    }
}

fn choose_explicit_color(
    options: &[(String, Option<String>, Option<String>)],
    seed_supports_color: Option<bool>,
) -> Option<(String, Option<(String, String)>)> {
    if seed_supports_color == Some(false) {
        return None;
    }
    if options.is_empty() {
        return Some(("psk:Color".to_string(), None));
    }
    if options
        .iter()
        .any(|(local, _, uri)| local == "Color" && uri.as_deref() == Some(NS_PSK))
    {
        return Some(("psk:Color".to_string(), None));
    }
    // Check for vendor-specific color option (anything other than Monochrome/Grayscale).
    for (local, pref, uri) in options {
        if local != "Monochrome" && local != "Grayscale" {
            if let (Some(p), Some(u)) = (pref, uri) {
                if u == NS_PSK {
                    return Some((format!("psk:{local}"), None));
                }
                return Some((format!("{p}:{local}"), Some((p.clone(), u.clone()))));
            }
        }
    }
    // Capabilities list only Monochrome/Grayscale -> physical B&W printer; omit Color safely.
    None
}

fn choose_explicit_monochrome(
    options: &[(String, Option<String>, Option<String>)],
) -> Option<(String, Option<(String, String)>)> {
    if options.is_empty() {
        return Some(("psk:Monochrome".to_string(), None));
    }
    if options
        .iter()
        .any(|(local, _, uri)| local == "Monochrome" && uri.as_deref() == Some(NS_PSK))
    {
        return Some(("psk:Monochrome".to_string(), None));
    }
    if options
        .iter()
        .any(|(local, _, uri)| local == "Grayscale" && uri.as_deref() == Some(NS_PSK))
    {
        return Some(("psk:Grayscale".to_string(), None));
    }
    for (local, pref, uri) in options {
        let low = local.to_ascii_lowercase();
        if low.contains("mono") || low.contains("gray") || low.contains("grey") || low.contains("black") {
            if let (Some(p), Some(u)) = (pref, uri) {
                if u == NS_PSK {
                    return Some((format!("psk:{local}"), None));
                }
                return Some((format!("{p}:{local}"), Some((p.clone(), u.clone()))));
            }
        }
    }
    None
}

/// Build the explicit Print Schema Delta XML for the negotiated job options.
pub(crate) fn build_delta_ticket_xml(
    copies: u32,
    collate: bool,
    duplex: Option<Duplex>,
    document_duplex: bool,
    landscape: bool,
    color_choice: Option<&(String, Option<(String, String)>)>,
    custom_paper_um: Option<(u32, u32)>,
) -> String {
    let copies = copies.clamp(1, 999);
    let collate_opt = if collate { "Collated" } else { "Uncollated" };
    let orient_opt = if landscape { "Landscape" } else { "Portrait" };
    let vendor_ns = color_choice
        .and_then(|(_, ns)| ns.as_ref())
        .filter(|(p, _)| !matches!(p.as_str(), "psf" | "psk" | "xsi" | "xsd"))
        .map(|(p, u)| format!(" xmlns:{p}=\"{u}\""))
        .unwrap_or_default();

    let duplex_xml = match duplex {
        None => String::new(),
        Some(d) => {
            let opt = match d {
                Duplex::Off => "OneSided",
                Duplex::LongEdge => "TwoSidedLongEdge",
                Duplex::ShortEdge => "TwoSidedShortEdge",
            };
            let feat = if document_duplex {
                "DocumentDuplex"
            } else {
                "JobDuplexAllDocumentsContiguously"
            };
            format!("\n <psf:Feature name=\"psk:{feat}\"><psf:Option name=\"psk:{opt}\"/></psf:Feature>")
        }
    };

    let color_xml = match color_choice {
        Some((qname, _)) => format!(
            "\n <psf:Feature name=\"psk:PageOutputColor\"><psf:Option name=\"{qname}\"/></psf:Feature>"
        ),
        None => String::new(),
    };

    let custom_media_xml = match custom_paper_um {
        Some((w, h)) => {
            let (min_u, max_u) = (w.min(h), w.max(h));
            format!(
                "\n <psf:Feature name=\"psk:PageMediaSize\"><psf:Option name=\"psk:CustomMediaSize\">\
                 <psf:ScoredProperty name=\"psk:MediaSizeWidth\"><psf:Value xsi:type=\"xsd:integer\">{min_u}</psf:Value></psf:ScoredProperty>\
                 <psf:ScoredProperty name=\"psk:MediaSizeHeight\"><psf:Value xsi:type=\"xsd:integer\">{max_u}</psf:Value></psf:ScoredProperty>\
                 </psf:Option></psf:Feature>\
                 \n <psf:ParameterInit name=\"psk:PageMediaSizeMediaSizeWidth\"><psf:Value xsi:type=\"xsd:integer\">{min_u}</psf:Value></psf:ParameterInit>\
                 \n <psf:ParameterInit name=\"psk:PageMediaSizeMediaSizeHeight\"><psf:Value xsi:type=\"xsd:integer\">{max_u}</psf:Value></psf:ParameterInit>"
            )
        }
        None => String::new(),
    };

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <psf:PrintTicket version=\"1\" xmlns:psf=\"{NS_PSF}\" xmlns:psk=\"{NS_PSK}\" \
         xmlns:xsi=\"{NS_XSI}\" xmlns:xsd=\"{NS_XSD}\"{vendor_ns}>\n \
         <psf:ParameterInit name=\"psk:JobCopiesAllDocuments\"><psf:Value xsi:type=\"xsd:integer\">{copies}</psf:Value></psf:ParameterInit>\n \
         <psf:Feature name=\"psk:DocumentCollate\"><psf:Option name=\"psk:{collate_opt}\"/></psf:Feature>{duplex_xml}{color_xml}{custom_media_xml}\n \
         <psf:Feature name=\"psk:PageScaling\"><psf:Option name=\"psk:None\"/></psf:Feature>\n \
         <psf:Feature name=\"psk:PageOrientation\"><psf:Option name=\"psk:{orient_opt}\"/></psf:Feature>\n\
         </psf:PrintTicket>"
    )
}

/// Extract `(width_um, height_um)` from a validated `PrintTicket` XML's `PageMediaSize`.
pub(crate) fn read_ticket_paper_um(ticket_xml: &str) -> Option<(u32, u32)> {
    let doc = roxmltree::Document::parse(ticket_xml).ok()?;
    let media = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name((NS_PSF, "Feature")) && node_is_psk_named(*n, "PageMediaSize"))?;
    let int_prop = |prop: &str| {
        media
            .descendants()
            .find(|n| {
                (n.has_tag_name((NS_PSF, "ScoredProperty")) || n.has_tag_name((NS_PSF, "Property")))
                    && node_is_psk_named(*n, prop)
            })
            .and_then(|n| n.children().find(|v| v.has_tag_name((NS_PSF, "Value"))))
            .and_then(|v| v.text())
            .and_then(|t| t.trim().parse::<u32>().ok())
    };
    int_prop("MediaSizeWidth")
        .zip(int_prop("MediaSizeHeight"))
        .filter(|&(w, h)| w > 0 && h > 0)
}

/// Whether `actual_um` matches `wanted_um` (orientation-independent) within `RETAINED_PAPER_TOLERANCE_UM`.
pub fn paper_matches_retained(actual_um: (u32, u32), wanted_um: (u32, u32)) -> bool {
    let (aw, ah) = (actual_um.0.min(actual_um.1), actual_um.0.max(actual_um.1));
    let (ww, wh) = (wanted_um.0.min(wanted_um.1), wanted_um.0.max(wanted_um.1));
    aw.abs_diff(ww) <= RETAINED_PAPER_TOLERANCE_UM && ah.abs_diff(wh) <= RETAINED_PAPER_TOLERANCE_UM
}

/// Whether `PrintCapabilities` XML offers `psk:CustomMediaSize` under `PageMediaSize`.
pub(crate) fn capabilities_offer_custom_media(capabilities_xml: &str) -> bool {
    let Ok(doc) = roxmltree::Document::parse(capabilities_xml) else {
        return false;
    };
    doc.root_element()
        .children()
        .filter(|n| n.has_tag_name((NS_PSF, "Feature")) && node_is_psk_named(*n, "PageMediaSize"))
        .flat_map(|f| f.children())
        .any(|o| o.has_tag_name((NS_PSF, "Option")) && node_is_psk_named(o, "CustomMediaSize"))
}

/// Whether `PrintCapabilities` XML supports hardware `DocumentCollate = Collated`.
pub(crate) fn capabilities_offer_collate(capabilities_xml: &str) -> Option<bool> {
    let doc = roxmltree::Document::parse(capabilities_xml).ok()?;
    let root = doc.root_element();
    if !root.has_tag_name((NS_PSF, "PrintCapabilities")) {
        return None;
    }
    Some(
        root.children()
            .filter(|n| {
                n.has_tag_name((NS_PSF, "Feature")) && node_is_psk_named(*n, "DocumentCollate")
            })
            .flat_map(|f| f.children())
            .any(|o| o.has_tag_name((NS_PSF, "Option")) && node_is_psk_named(o, "Collated")),
    )
}

/// Rewrite Windows `ns0000` vendor prefixes back to the driver's own declared prefixes.
pub(crate) fn respell_vendor_prefixes(ticket: &str, driver_default: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(ticket).ok()?;
    let driver_doc = roxmltree::Document::parse(driver_default).ok()?;
    let vendor_ns = |d: &roxmltree::Document<'_>| -> Vec<(String, String)> {
        d.root_element()
            .namespaces()
            .filter_map(|n| n.name().map(|p| (p.to_string(), n.uri().to_string())))
            .filter(|(_, u)| ![NS_PSF, NS_PSK, NS_XSI, NS_XSD, NS_XML].contains(&u.as_str()))
            .collect()
    };
    let own = vendor_ns(&driver_doc);
    let declared = vendor_ns(&doc);
    let renames: Vec<(&str, &str)> = declared
        .iter()
        .filter_map(|(old, uri)| {
            own.iter()
                .find(|(_, u)| u == uri)
                .map(|(new, _)| (old.as_str(), new.as_str()))
        })
        .filter(|(old, new)| old != new)
        .collect();
    if renames.is_empty() {
        return None;
    }
    let mut out = ticket.to_string();
    for (old, new) in &renames {
        out = out
            .replace(&format!("xmlns:{old}="), &format!("xmlns:{new}="))
            .replace(&format!("\"{old}:"), &format!("\"{new}:"))
            .replace(&format!("'{old}:"), &format!("'{new}:"))
            .replace(&format!(">{old}:"), &format!(">{new}:"));
    }
    roxmltree::Document::parse(&out).is_ok().then_some(out)
}

/// Sanitize a job title into a safe temporary PDF filename (avoiding Windows device names).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn safe_job_filename(title: &str) -> String {
    let stem = title
        .strip_suffix(".pdf")
        .or_else(|| title.strip_suffix(".PDF"))
        .unwrap_or(title);
    let safe: String = stem
        .chars()
        .take(100)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_end_matches(['.', ' ']).trim_start();
    let reserved = safe.split('.').next().map(str::trim_end).is_some_and(|base| {
        let up = base.to_ascii_uppercase();
        matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((up.starts_with("COM") || up.starts_with("LPT"))
                && up.len() == 4
                && up.ends_with(|c: char| c.is_ascii_digit()))
    });
    match (safe.is_empty(), reserved) {
        (true, _) => "Print.pdf".to_string(),
        (false, true) => format!("_{safe}.pdf"),
        (false, false) => format!("{safe}.pdf"),
    }
}

/// Reframe or orient PDF sheets onto `target_paper_pt` when `ShrinkToFit` adaptation or
/// mixed-orientation sheet alignment is needed, and optionally repeat sheets for software collation.
pub(crate) fn prepare_pdf_for_ticket(
    pdf_bytes: &[u8],
    paper_um: (u32, u32),
    landscape: bool,
    shrink_to_fit: bool,
    repeat_copies: Option<(u32, bool)>,
) -> Result<Option<Vec<u8>>, PrintError> {
    let doc = pdfcraft_cos::Document::open(Arc::new(pdf_bytes.to_vec()))
        .map_err(|e| PrintError::Spool(format!("the print-ready PDF could not be read: {e}")))?;
    let pages = pdfcraft_model::pages(&doc);
    if pages.is_empty() {
        return Ok(None);
    }
    let portrait_paper = (
        um_to_pts(paper_um.0.min(paper_um.1)).max(72.0),
        um_to_pts(paper_um.0.max(paper_um.1)).max(72.0),
    );
    let mixed_orientation = pages.iter().any(|p| {
        let (w, h) = p.display_size(&doc);
        (w > h + SHEET_SAME_PT) != landscape && (w - h).abs() > SHEET_SAME_PT
    });

    let mut working_bytes: Option<Vec<u8>> = None;
    if shrink_to_fit || mixed_orientation {
        let settings = Settings {
            pages: (0..pages.len()).collect(),
            paper: portrait_paper,
            orientation: if landscape {
                Orientation::Landscape
            } else {
                Orientation::Portrait
            },
            layout: Layout::Size(SizeMode::Shrink),
            content: Content::DocumentAndMarkups,
            ..Settings::default()
        };
        working_bytes = Some(impose(&doc, &settings)?);
    }

    if let Some((copies, two_sided)) = repeat_copies.filter(|&(c, _)| c > 1) {
        let base = working_bytes.as_deref().unwrap_or(pdf_bytes);
        if let Some(repeated) = repeat_pdf_sheets(base, copies, two_sided)? {
            working_bytes = Some(repeated);
        }
    }
    Ok(working_bytes)
}

fn repeat_pdf_sheets(
    pdf_bytes: &[u8],
    copies: u32,
    two_sided: bool,
) -> Result<Option<Vec<u8>>, PrintError> {
    let mut doc = pdfcraft_cos::Document::open(Arc::new(pdf_bytes.to_vec()))
        .map_err(|e| PrintError::Spool(format!("the print-ready PDF could not be read: {e}")))?;
    let pages = pdfcraft_model::pages(&doc);
    if pages.is_empty() {
        return Ok(None);
    }
    let Some(pages_root) = doc
        .root()
        .and_then(|r| doc.get(r).as_dict().and_then(|d| d.reference(b"Pages")))
    else {
        return Ok(None);
    };
    let mut single_copy_refs: Vec<pdfcraft_cos::Object> =
        pages.iter().map(|p| pdfcraft_cos::Object::Ref(p.obj)).collect();
    if two_sided && single_copy_refs.len() % 2 == 1 {
        let (w, h) = pages[0].display_size(&doc);
        let mut blank = pdfcraft_cos::Dict::new();
        blank.set(b"Type".to_vec(), pdfcraft_cos::Object::name("Page"));
        blank.set(b"Parent".to_vec(), pdfcraft_cos::Object::Ref(pages_root));
        blank.set(
            b"MediaBox".to_vec(),
            pdfcraft_cos::Object::Array(
                [0.0, 0.0, w, h]
                    .into_iter()
                    .map(pdfcraft_cos::Object::Real)
                    .collect(),
            ),
        );
        let blank_ref = doc.add(pdfcraft_cos::Object::Dict(blank));
        single_copy_refs.push(pdfcraft_cos::Object::Ref(blank_ref));
    }
    let copies = copies.clamp(1, 999) as usize;
    let total_kids: Vec<pdfcraft_cos::Object> = std::iter::repeat_n(single_copy_refs, copies)
        .flatten()
        .collect();
    let total_count = total_kids.len() as i64;
    for page in &pages {
        let _ = doc.update_dict(page.obj, |d| {
            d.set(b"Parent".to_vec(), pdfcraft_cos::Object::Ref(pages_root));
        });
    }
    doc.update_dict(pages_root, |d| {
        d.set(b"Kids".to_vec(), pdfcraft_cos::Object::Array(total_kids));
        d.set(b"Count".to_vec(), pdfcraft_cos::Object::Int(total_count));
    })
    .map_err(|e| PrintError::Spool(format!("could not repeat collated pages: {e}")))?;
    pdfcraft_cos::write_full(&doc, &pdfcraft_cos::SaveOptions::default())
        .map(Some)
        .map_err(|e| PrintError::Spool(format!("could not write collated PDF: {e}")))
}

#[cfg(windows)]
mod imp {
    use std::io::Write as _;

    use winprint::printer::{PrinterDevice, WinPdfPrinter};
    use winprint::ticket::document::{
        NS_PSK, OwnedName, PrintCapabilitiesDocument, PrintFeature, PrintTicketDocument,
        reader::ParsableXmlDocument as _,
    };
    use winprint::ticket::{
        FeatureOptionPack as _, PageMediaSize, PrintCapabilities, PrintTicket, PrintTicketBuilder,
    };

    use super::{
        DriverPaperCandidate, JobSheetTarget, WindowsNormalizedSeed, build_delta_ticket_xml,
        capabilities_offer_collate, capabilities_offer_custom_media, capabilities_support_duplex,
        inspect_job_target, match_driver_paper, negotiate_duplex, paper_matches_retained,
        prefers_document_duplex, prepare_pdf_for_ticket, read_ticket_paper_um,
        respell_vendor_prefixes, safe_job_filename, select_color_option_xml,
    };
    use crate::PrintError;
    use crate::spool::{Duplex, Job, Scaling, with_ticket_provider_lock};

    fn spool_err(e: impl std::fmt::Display) -> PrintError {
        PrintError::Spool(e.to_string())
    }

    fn select_printer_device(requested: Option<&str>) -> Result<PrinterDevice, PrintError> {
        let devices = PrinterDevice::all().map_err(spool_err)?;
        let default_name = craft_print::rgb::default_printer().ok().flatten();
        let target = requested
            .or(default_name.as_deref())
            .ok_or_else(|| spool_err("there is no default printer; choose a printer"))?;
        devices.into_iter().find(|d| d.name() == target).ok_or_else(|| {
            spool_err(format!(
                "the printer is not available: {target} (reopen Print to refresh the list)"
            ))
        })
    }

    struct ResolvedTicketPlan {
        ticket: PrintTicket,
        paper_um: (u32, u32),
        shrink_to_fit: bool,
        effective_duplex: Option<Duplex>,
        driver_collates: Option<bool>,
    }

    fn build_printer_ticket(
        device: &PrinterDevice,
        job: &Job,
        target: &JobSheetTarget,
        seed: Option<&WindowsNormalizedSeed>,
        copies_override: Option<u32>,
    ) -> Result<ResolvedTicketPlan, PrintError> {
        let caps_bytes = with_ticket_provider_lock(|| {
            PrintCapabilities::fetch_xml_for_ticket(device, None)
        })
        .map_err(spool_err)?;
        let caps_xml = std::str::from_utf8(&caps_bytes).map_err(spool_err)?;
        let caps_doc = PrintCapabilitiesDocument::parse_from_bytes(&caps_bytes).map_err(spool_err)?;
        let caps = PrintCapabilities { document: caps_doc };

        let medias: Vec<PageMediaSize> = caps.page_media_sizes().collect();
        let candidates: Vec<DriverPaperCandidate<'_>> = medias
            .iter()
            .map(|m| DriverPaperCandidate {
                width_um: m.size().width_in_micron(),
                height_um: m.size().height_in_micron(),
                display_name: m.display_name(),
                is_psk_keyword: m
                    .option()
                    .name
                    .as_ref()
                    .is_some_and(|n| n.namespace_ref() == Some(NS_PSK)),
            })
            .collect();

        let matched_media =
            match_driver_paper(&candidates, target.target_paper_um).and_then(|i| medias.get(i));
        let custom_media_um = if matched_media.is_none() && capabilities_offer_custom_media(caps_xml)
        {
            Some(target.target_paper_um)
        } else {
            None
        };

        let caps_duplex = capabilities_support_duplex(caps_xml);
        let seed_duplex = seed.and_then(|s| s.supports_duplex);
        let requested_duplex = seed.and_then(|s| s.effective_duplex).or(job.duplex);
        let effective_duplex = negotiate_duplex(requested_duplex, seed_duplex, caps_duplex);
        let doc_duplex = prefers_document_duplex(caps_xml, effective_duplex);

        let seed_color = seed.and_then(|s| s.supports_color);
        let color_choice = select_color_option_xml(caps_xml, target.color_intent, seed_color);

        let copies = copies_override.unwrap_or(job.copies).clamp(1, 999);
        let delta_xml = build_delta_ticket_xml(
            copies,
            job.collate,
            effective_duplex,
            doc_duplex,
            target.landscape,
            color_choice.as_ref(),
            custom_media_um,
        );

        let mut delta_doc =
            PrintTicketDocument::parse_from_bytes(delta_xml.as_bytes()).map_err(spool_err)?;
        if let Some(media) = matched_media {
            let media_ticket: PrintTicket = media.clone().into();
            let media_doc =
                PrintTicketDocument::parse_from_bytes(media_ticket.get_xml()).map_err(spool_err)?;
            delta_doc.features.extend(media_doc.features);
            delta_doc.parameter_inits.extend(media_doc.parameter_inits);
        }
        apply_no_scaling_option(&caps, &mut delta_doc);

        let merged = merge_with_driver(
            device,
            seed.and_then(|s| s.base_ticket_xml.as_deref()),
            PrintTicket::from(delta_doc),
        )?;
        let merged_xml = std::str::from_utf8(merged.get_xml()).map_err(spool_err)?;
        let actual_paper_um = read_ticket_paper_um(merged_xml)
            .or_else(|| seed.and_then(|s| s.retained_paper_um))
            .unwrap_or(target.target_paper_um);

        let seed_adapted = seed.is_some_and(|s| s.adapted_shrink_to_fit);
        let paper_substituted = !paper_matches_retained(actual_paper_um, target.target_paper_um);
        let shrink_to_fit =
            job.scaling == Scaling::ShrinkToFit || seed_adapted || paper_substituted;

        Ok(ResolvedTicketPlan {
            ticket: merged,
            paper_um: actual_paper_um,
            shrink_to_fit,
            effective_duplex,
            driver_collates: capabilities_offer_collate(caps_xml),
        })
    }

    fn apply_no_scaling_option(caps: &PrintCapabilities, doc: &mut PrintTicketDocument) {
        let scaling_name = OwnedName::qualified("PageScaling", NS_PSK, Some("psk"));
        if let Some(no_scaling) = caps
            .options_for_feature(scaling_name.clone())
            .find(|o| o.name.as_ref().is_some_and(|n| n.local_name == "None"))
        {
            doc.features.retain(|f| {
                !(f.name.local_name == "PageScaling" && f.name.namespace_ref() == Some(NS_PSK))
            });
            doc.features.push(PrintFeature {
                name: scaling_name,
                properties: Vec::new(),
                options: vec![no_scaling.clone()],
                features: Vec::new(),
            });
        }
    }

    fn merge_with_driver(
        device: &PrinterDevice,
        base_ticket_xml: Option<&[u8]>,
        delta: PrintTicket,
    ) -> Result<PrintTicket, PrintError> {
        with_ticket_provider_lock(|| {
            let mut builder = PrintTicketBuilder::new(device).map_err(spool_err)?;
            if let Some(base_xml) = base_ticket_xml {
                // Best-effort base seed from PTConvertDevModeToPrintTicket; if a driver rejects
                // its own converted snapshot, keep the clean PrintTicketBuilder base.
                let _ = builder.merge(PrintTicket::from_xml(base_xml.to_vec()));
            }
            builder.merge(delta).map_err(spool_err)?;
            let built = builder.build().map_err(spool_err)?;

            let default_builder = PrintTicketBuilder::new(device).map_err(spool_err)?;
            let own_xml = default_builder
                .build()
                .ok()
                .and_then(|t| String::from_utf8(t.into_xml()).ok());
            if let (Ok(ticket_str), Some(driver_str)) =
                (std::str::from_utf8(built.get_xml()), own_xml.as_deref())
                && let Some(respelled) = respell_vendor_prefixes(ticket_str, driver_str)
            {
                return Ok(PrintTicket::from_xml(respelled.into_bytes()));
            }
            Ok(built)
        })
    }

    pub(crate) fn submit_with_seed_impl(
        pdf_bytes: &[u8],
        job: &Job,
        seed: Option<&WindowsNormalizedSeed>,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<String, PrintError> {
        let target = inspect_job_target(pdf_bytes, job)?;
        let device = select_printer_device(job.printer.as_deref())?;
        let dir = tempfile::Builder::new()
            .prefix("pdfcraft-print-")
            .tempdir()
            .map_err(|e| spool_err(format!("the print job could not be staged: {e}")))?;
        let path = dir.path().join(safe_job_filename(&job.title));

        let mut plan = build_printer_ticket(&device, job, &target, seed, None)?;
        let copies = job.copies.clamp(1, 999);
        let needs_sw_collate =
            job.collate && copies > 1 && target.sheet_count > 1 && plan.driver_collates == Some(false);
        let repeat_copies = if needs_sw_collate {
            plan = build_printer_ticket(&device, job, &target, seed, Some(1))?;
            let two_sided = matches!(
                plan.effective_duplex,
                Some(Duplex::LongEdge | Duplex::ShortEdge)
            );
            Some((copies, two_sided))
        } else {
            None
        };

        let updated = prepare_pdf_for_ticket(
            pdf_bytes,
            plan.paper_um,
            target.landscape,
            plan.shrink_to_fit,
            repeat_copies,
        )?;
        let final_bytes = updated.as_deref().unwrap_or(pdf_bytes);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| spool_err(format!("the print job could not be written: {e}")))?;
        file.write_all(final_bytes)
            .map_err(|e| spool_err(format!("the print job could not be written: {e}")))?;
        drop(file);

        // Run WinRT/COM printing on a dedicated worker thread so COM apartment state is isolated,
        // while releasing PRINT_TICKET_PROVIDER immediately upon `progress(0, total)`.
        let ticket = plan.ticket;
        let path_clone = path.clone();
        let (tx, rx) = std::sync::mpsc::channel::<(usize, usize)>();
        let worker = std::thread::spawn(move || -> Result<(), PrintError> {
            let mut provider_guard = Some(crate::spool::print_ticket_provider());
            ticket.to_dev_mode(&device).map_err(spool_err)?;
            let printer = WinPdfPrinter::new(device);
            printer
                .print_with_progress(&path_clone, ticket, |processed, total| {
                    // Release PrintTicket provider lock as soon as DxgiPrintContext is created
                    // (`processed == 0`), allowing all other printers to spool concurrently!
                    drop(provider_guard.take());
                    let _ = tx.send((processed as usize, total as usize));
                })
                .map_err(|e| {
                    let mut msg = e.to_string();
                    let mut cause = std::error::Error::source(&e);
                    while let Some(src) = cause {
                        msg.push_str(&format!(": {src}"));
                        cause = src.source();
                    }
                    spool_err(msg)
                })
        });

        while let Ok((sent, total)) = rx.recv() {
            progress(sent, total);
        }
        let outcome = worker
            .join()
            .map_err(|_| spool_err("the print worker thread panicked unexpectedly"))?;
        drop(dir);
        outcome?;

        Ok(match repeat_copies {
            Some((c, _)) => format!("spooled {} sheet(s) {c} times, collated", target.sheet_count),
            None => format!("spooled {} sheet(s)", target.sheet_count),
        })
    }
}

/// Submit a print-ready PDF on Windows using an optional normalized `DEVMODEW` + `PrintTicket` seed.
#[cfg(windows)]
pub fn submit_with_seed(
    pdf_bytes: &[u8],
    job: &Job,
    seed: Option<&WindowsNormalizedSeed>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<String, PrintError> {
    imp::submit_with_seed_impl(pdf_bytes, job, seed, progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_specs_stamp_and_strip_cleanly() {
        let mut job = Job {
            options: vec![("InputSlot".into(), "Tray2".into())],
            ..Job::default()
        };
        apply_broadcast_specs(&mut job, true, (210_000, 297_000), Some(false));
        assert!(!job.grayscale);
        assert_eq!(job_color_intent(&job), ColorIntent::ExplicitColor);
        assert_eq!(job_paper_um(&job), Some((210_000, 297_000)));
        assert_eq!(job_explicit_landscape(&job), Some(false));

        // Calling again overwrites rather than duplicating `pdfcraft:*` keys.
        apply_broadcast_specs(&mut job, false, (297_000, 420_000), Some(true));
        assert!(job.grayscale);
        assert_eq!(job_color_intent(&job), ColorIntent::ExplicitMonochrome);
        assert_eq!(job_paper_um(&job), Some((297_000, 420_000)));
        assert_eq!(job_explicit_landscape(&job), Some(true));
        assert_eq!(
            job.options
                .iter()
                .filter(|(k, _)| k.starts_with("pdfcraft:"))
                .count(),
            3
        );

        // Stripping for CUPS removes all `pdfcraft:*` options while preserving real CUPS options.
        let cups_job = strip_internal_options(&job);
        assert_eq!(cups_job.options, vec![("InputSlot".into(), "Tray2".into())]);
    }

    #[test]
    fn virtual_save_printers_are_detected_case_insensitively() {
        for virtual_name in [
            "Microsoft Print to PDF",
            "microsoft print to pdf",
            "Microsoft XPS Document Writer",
            "OneNote for Windows 10",
            "Send To OneNote 2016",
            "Fax",
            "Microsoft Shared Fax Driver",
        ] {
            assert!(
                is_virtual_save_queue(virtual_name),
                "expected virtual save printer: {virtual_name}"
            );
        }
        for physical_name in [
            "HP LaserJet Pro MFP M428fdw",
            "Brother HL-L2350DW series",
            "EPSON EcoTank L3250",
            "Canon iR-ADV C5535",
        ] {
            assert!(
                !is_virtual_save_queue(physical_name),
                "expected physical printer: {physical_name}"
            );
        }
    }

    #[test]
    fn explicit_color_emits_psk_color_and_degrades_on_bw_only_printer() {
        let color_caps = format!(
            r#"<psf:PrintCapabilities version="1" xmlns:psf="{NS_PSF}" xmlns:psk="{NS_PSK}">
                <psf:Feature name="psk:PageOutputColor">
                    <psf:Option name="psk:Color"/>
                    <psf:Option name="psk:Monochrome"/>
                </psf:Feature>
            </psf:PrintCapabilities>"#
        );
        let choice = select_color_option_xml(&color_caps, ColorIntent::ExplicitColor, Some(true));
        assert_eq!(choice, Some(("psk:Color".to_string(), None)));
        let xml = build_delta_ticket_xml(
            2,
            true,
            Some(Duplex::LongEdge),
            false,
            false,
            choice.as_ref(),
            None,
        );
        assert!(xml.contains(
            "<psf:Feature name=\"psk:PageOutputColor\"><psf:Option name=\"psk:Color\"/></psf:Feature>"
        ));

        // Vendor-only color option (e.g. Epson/Brother custom namespace)
        let vendor_caps = format!(
            r#"<psf:PrintCapabilities version="1" xmlns:psf="{NS_PSF}" xmlns:psk="{NS_PSK}" xmlns:ep="urn:epson:color">
                <psf:Feature name="psk:PageOutputColor">
                    <psf:Option name="ep:VividColor"/>
                    <psf:Option name="psk:Grayscale"/>
                </psf:Feature>
            </psf:PrintCapabilities>"#
        );
        let v_choice = select_color_option_xml(&vendor_caps, ColorIntent::ExplicitColor, None);
        assert_eq!(
            v_choice,
            Some((
                "ep:VividColor".to_string(),
                Some(("ep".to_string(), "urn:epson:color".to_string()))
            ))
        );

        // B&W-only laser printer: ExplicitColor safely omits PageOutputColor instead of failing!
        let bw_only_caps = format!(
            r#"<psf:PrintCapabilities version="1" xmlns:psf="{NS_PSF}" xmlns:psk="{NS_PSK}">
                <psf:Feature name="psk:PageOutputColor">
                    <psf:Option name="psk:Monochrome"/>
                </psf:Feature>
            </psf:PrintCapabilities>"#
        );
        assert_eq!(
            select_color_option_xml(&bw_only_caps, ColorIntent::ExplicitColor, None),
            None
        );
        assert_eq!(
            select_color_option_xml(&color_caps, ColorIntent::ExplicitColor, Some(false)),
            None
        );

        // ExplicitMonochrome selects Grayscale when Monochrome is absent
        assert_eq!(
            select_color_option_xml(&vendor_caps, ColorIntent::ExplicitMonochrome, Some(true)),
            Some(("psk:Grayscale".to_string(), None))
        );
    }

    #[test]
    fn paper_matching_uses_1mm_tolerance_and_supports_custom_media_xml() {
        let candidates = vec![
            DriverPaperCandidate {
                width_um: 215_900,
                height_um: 279_400,
                display_name: Some("Letter"),
                is_psk_keyword: true,
            },
            DriverPaperCandidate {
                width_um: 210_000,
                height_um: 297_000,
                display_name: Some("A4 210 x 297 mm"),
                is_psk_keyword: true,
            },
        ];
        // 0.8 mm difference (800 µm <= 1000 µm) matches A4!
        assert_eq!(
            match_driver_paper(&candidates, (210_800, 296_400)),
            Some(1)
        );
        // Orientation-independent match (Landscape A4 -> index 1)
        assert_eq!(
            match_driver_paper(&candidates, (297_000, 210_000)),
            Some(1)
        );
        // Custom 140 x 200 mm -> None (triggers CustomMediaSize / DMPAPER_USER)
        assert_eq!(match_driver_paper(&candidates, (140_000, 200_000)), None);

        let custom_xml = build_delta_ticket_xml(
            1,
            true,
            Some(Duplex::Off),
            false,
            false,
            None,
            Some((140_000, 200_000)),
        );
        assert!(custom_xml.contains("psk:CustomMediaSize"));
        assert!(custom_xml.contains(">140000<"));
        assert!(custom_xml.contains(">200000<"));
        assert_eq!(read_ticket_paper_um(&custom_xml), Some((140_000, 200_000)));
    }

    #[test]
    fn simplex_only_printer_degrades_duplex_to_simplex_without_aborting() {
        let simplex_caps = format!(
            r#"<psf:PrintCapabilities version="1" xmlns:psf="{NS_PSF}" xmlns:psk="{NS_PSK}">
                <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
                    <psf:Option name="psk:OneSided"/>
                </psf:Feature>
            </psf:PrintCapabilities>"#
        );
        let caps_support = capabilities_support_duplex(&simplex_caps);
        assert_eq!(caps_support, Some(false));
        assert_eq!(
            negotiate_duplex(Some(Duplex::LongEdge), None, caps_support),
            Some(Duplex::Off)
        );
        assert_eq!(
            negotiate_duplex(Some(Duplex::ShortEdge), Some(false), Some(true)),
            Some(Duplex::Off)
        );
        assert_eq!(
            negotiate_duplex(Some(Duplex::LongEdge), Some(true), Some(true)),
            Some(Duplex::LongEdge)
        );
    }

    #[test]
    fn devmode_template_capture_extracts_all_normalized_fields() {
        let mut dm = vec![0u8; 220 + 64];
        dm[68..70].copy_from_slice(&220u16.to_le_bytes());
        dm[70..72].copy_from_slice(&64u16.to_le_bytes());
        // DM_ORIENTATION | DM_PAPERLENGTH | DM_PAPERWIDTH | DM_COPIES | DM_COLOR | DM_DUPLEX | DM_COLLATE
        let flags: u32 = 0x0001 | 0x0004 | 0x0008 | 0x0100 | 0x0800 | 0x1000 | 0x8000;
        dm[72..76].copy_from_slice(&flags.to_le_bytes());
        dm[76..78].copy_from_slice(&2i16.to_le_bytes()); // Landscape
        dm[80..82].copy_from_slice(&2970i16.to_le_bytes()); // 297.0 mm length
        dm[82..84].copy_from_slice(&2100i16.to_le_bytes()); // 210.0 mm width
        dm[86..88].copy_from_slice(&4i16.to_le_bytes()); // 4 copies
        dm[92..94].copy_from_slice(&2i16.to_le_bytes()); // DMCOLOR_COLOR -> grayscale = false
        dm[94..96].copy_from_slice(&2i16.to_le_bytes()); // DMDUP_VERTICAL -> LongEdge
        dm[100..102].copy_from_slice(&1i16.to_le_bytes()); // Collate = true

        let captured = parse_devmode_template_bytes(&dm, None, &[]).unwrap();
        assert_eq!(captured.paper_um, Some((210_000, 297_000)));
        assert_eq!(captured.paper_name.as_deref(), Some("A4"));
        assert_eq!(captured.grayscale, Some(false));
        assert_eq!(captured.duplex, Some(Duplex::LongEdge));
        assert_eq!(captured.landscape, Some(true));
        assert_eq!(captured.copies, Some(4));
        assert_eq!(captured.collate, Some(true));
    }
}
