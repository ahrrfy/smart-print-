//! Duplex binding edge control and Sheet Master Orientation Alignment.
//!
//! Provides hardware driver duplex binding representations (Win32 DEVMODE, V4 XPS PrintTicket,
//! CUPS/IPP 2.0) and sheet pairing algorithms to align mixed-orientation back pages (preventing
//! inverted or clipped printing).

/// Hardware duplex binding edge selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DuplexBinding {
    #[default]
    Simplex,
    /// Standard booklet binding: flip on long edge (Win32 DMDUP_VERTICAL / psk:TwoSidedLongEdge).
    FlipLongEdge,
    /// Notepad/calendar binding: flip on short edge (Win32 DMDUP_HORIZONTAL / psk:TwoSidedShortEdge).
    FlipShortEdge,
}

impl DuplexBinding {
    /// Returns true if this binding is double-sided (long edge or short edge).
    pub fn is_two_sided(self) -> bool {
        matches!(self, Self::FlipLongEdge | Self::FlipShortEdge)
    }

    /// Converts to Win32 GDI `dmDuplex` value.
    pub fn to_devmode_duplex(self) -> i16 {
        match self {
            Self::Simplex => 1,       // DMDUP_SIMPLEX
            Self::FlipLongEdge => 2,  // DMDUP_VERTICAL
            Self::FlipShortEdge => 3, // DMDUP_HORIZONTAL
        }
    }

    /// Parses from Win32 GDI `dmDuplex` value.
    pub fn from_devmode_duplex(val: i16) -> Option<Self> {
        match val {
            1 => Some(Self::Simplex),
            2 => Some(Self::FlipLongEdge),
            3 => Some(Self::FlipShortEdge),
            _ => None,
        }
    }

    /// Converts to V4 XPS PrintTicket option name.
    pub fn to_printticket_option(self) -> &'static str {
        match self {
            Self::Simplex => "psk:OneSided",
            Self::FlipLongEdge => "psk:TwoSidedLongEdge",
            Self::FlipShortEdge => "psk:TwoSidedShortEdge",
        }
    }

    /// Converts to CUPS/IPP sides attribute string.
    pub fn to_ipp_sides(self) -> &'static str {
        match self {
            Self::Simplex => "one-sided",
            Self::FlipLongEdge => "two-sided-long-edge",
            Self::FlipShortEdge => "two-sided-short-edge",
        }
    }
}

/// A physical sheet pair for double-sided printing.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplexSheetPair {
    pub sheet_index: usize,
    pub front_page: usize,
    pub back_page: Option<usize>,
    pub master_is_portrait: bool,
    pub back_needs_rotation: bool,
    pub back_rotation_deg: u16,
}

/// Determines alignment rotation for a single front/back page pair.
///
/// The front page establishes the master orientation for the physical sheet.
/// When the back page has a conflicting orientation (e.g. Landscape on a Portrait sheet),
/// it is rotated by 90° or 270° depending on the binding edge.
pub fn align_sheet_pair_orientation(
    front_size: (f64, f64),
    back_size: (f64, f64),
    binding: DuplexBinding,
) -> (bool, u16) {
    let master_is_portrait = front_size.1 >= front_size.0;
    let back_is_portrait = back_size.1 >= back_size.0;

    if !binding.is_two_sided() || master_is_portrait == back_is_portrait {
        (master_is_portrait, 0)
    } else {
        let rotation = match (master_is_portrait, binding) {
            (true, DuplexBinding::FlipLongEdge) => 270,
            (true, DuplexBinding::FlipShortEdge) => 90,
            (false, DuplexBinding::FlipLongEdge) => 90,
            (false, DuplexBinding::FlipShortEdge) => 270,
            _ => 0,
        };
        (master_is_portrait, rotation)
    }
}

/// Plans all physical sheets for a multi-page document under duplex settings.
pub fn plan_duplex_sheets(
    page_sizes: &[(f64, f64)],
    binding: DuplexBinding,
) -> Vec<DuplexSheetPair> {
    if page_sizes.is_empty() {
        return Vec::new();
    }

    if !binding.is_two_sided() {
        return page_sizes
            .iter()
            .enumerate()
            .map(|(idx, &size)| DuplexSheetPair {
                sheet_index: idx,
                front_page: idx,
                back_page: None,
                master_is_portrait: size.1 >= size.0,
                back_needs_rotation: false,
                back_rotation_deg: 0,
            })
            .collect();
    }

    let mut sheets = Vec::new();
    let total_pages = page_sizes.len();
    let mut p = 0;
    let mut sheet_idx = 0;

    while p < total_pages {
        let front = p;
        let front_size = page_sizes[front];
        let master_is_portrait = front_size.1 >= front_size.0;

        let (back_page, back_needs_rotation, back_rotation_deg) = if p + 1 < total_pages {
            let back = p + 1;
            let back_size = page_sizes[back];
            let (_, rot) = align_sheet_pair_orientation(front_size, back_size, binding);
            (Some(back), rot != 0, rot)
        } else {
            (None, false, 0)
        };

        sheets.push(DuplexSheetPair {
            sheet_index: sheet_idx,
            front_page: front,
            back_page,
            master_is_portrait,
            back_needs_rotation,
            back_rotation_deg,
        });

        p += 2;
        sheet_idx += 1;
    }

    sheets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_homogeneous_portrait_no_rotation() {
        let (master_portrait, rot) = align_sheet_pair_orientation(
            (595.0, 842.0),
            (595.0, 842.0),
            DuplexBinding::FlipLongEdge,
        );
        assert!(master_portrait);
        assert_eq!(rot, 0);
    }

    #[test]
    fn test_homogeneous_landscape_no_rotation() {
        let (master_portrait, rot) = align_sheet_pair_orientation(
            (842.0, 595.0),
            (842.0, 595.0),
            DuplexBinding::FlipShortEdge,
        );
        assert!(!master_portrait);
        assert_eq!(rot, 0);
    }

    #[test]
    fn test_mixed_portrait_front_landscape_back_long_edge() {
        let (master_portrait, rot) = align_sheet_pair_orientation(
            (595.0, 842.0),
            (842.0, 595.0),
            DuplexBinding::FlipLongEdge,
        );
        assert!(master_portrait);
        assert_eq!(rot, 270);
    }

    #[test]
    fn test_mixed_portrait_front_landscape_back_short_edge() {
        let (master_portrait, rot) = align_sheet_pair_orientation(
            (595.0, 842.0),
            (842.0, 595.0),
            DuplexBinding::FlipShortEdge,
        );
        assert!(master_portrait);
        assert_eq!(rot, 90);
    }

    #[test]
    fn test_mixed_landscape_front_portrait_back_long_edge() {
        let (master_portrait, rot) = align_sheet_pair_orientation(
            (842.0, 595.0),
            (595.0, 842.0),
            DuplexBinding::FlipLongEdge,
        );
        assert!(!master_portrait);
        assert_eq!(rot, 90);
    }

    #[test]
    fn test_mixed_landscape_front_portrait_back_short_edge() {
        let (master_portrait, rot) = align_sheet_pair_orientation(
            (842.0, 595.0),
            (595.0, 842.0),
            DuplexBinding::FlipShortEdge,
        );
        assert!(!master_portrait);
        assert_eq!(rot, 270);
    }

    #[test]
    fn test_simplex_mode_never_rotates() {
        let (_, rot) = align_sheet_pair_orientation(
            (595.0, 842.0),
            (842.0, 595.0),
            DuplexBinding::Simplex,
        );
        assert_eq!(rot, 0);
    }

    #[test]
    fn test_plan_duplex_sheets_odd_count() {
        let pages = vec![(595.0, 842.0), (842.0, 595.0), (595.0, 842.0)];
        let sheets = plan_duplex_sheets(&pages, DuplexBinding::FlipLongEdge);
        assert_eq!(sheets.len(), 2);

        // Sheet 0: Front 0 (portrait), Back 1 (landscape -> 270 rot)
        assert_eq!(sheets[0].sheet_index, 0);
        assert_eq!(sheets[0].front_page, 0);
        assert_eq!(sheets[0].back_page, Some(1));
        assert!(sheets[0].master_is_portrait);
        assert!(sheets[0].back_needs_rotation);
        assert_eq!(sheets[0].back_rotation_deg, 270);

        // Sheet 1: Front 2, Back None
        assert_eq!(sheets[1].sheet_index, 1);
        assert_eq!(sheets[1].front_page, 2);
        assert_eq!(sheets[1].back_page, None);
        assert!(!sheets[1].back_needs_rotation);
    }

    #[test]
    fn test_plan_duplex_sheets_simplex() {
        let pages = vec![(595.0, 842.0), (842.0, 595.0)];
        let sheets = plan_duplex_sheets(&pages, DuplexBinding::Simplex);
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].back_page, None);
        assert_eq!(sheets[1].back_page, None);
    }

    #[test]
    fn test_round_trip_devmode_and_ipp_conversion() {
        for b in [DuplexBinding::Simplex, DuplexBinding::FlipLongEdge, DuplexBinding::FlipShortEdge] {
            let dm = b.to_devmode_duplex();
            assert_eq!(DuplexBinding::from_devmode_duplex(dm), Some(b));
            assert!(!b.to_printticket_option().is_empty());
            assert!(!b.to_ipp_sides().is_empty());
        }
        assert_eq!(DuplexBinding::from_devmode_duplex(99), None);
    }
}
