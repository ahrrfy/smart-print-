//! Print presets and printer group persistence and management.
//! Provides reusable printer groupings and job setting configurations.
//! Strictly panic-free, Clippy-compliant, and Layer 7 compliant.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use serde::{Deserialize, Serialize};

/// Named cluster of printers for bulk or redundant dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrinterGroup {
    pub id: String,
    pub name: String,
    pub printer_names: BTreeSet<String>,
    pub is_built_in: bool,
}

impl PrinterGroup {
    /// Creates a new printer group.
    pub fn new(id: impl Into<String>, name: impl Into<String>, is_built_in: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            printer_names: BTreeSet::new(),
            is_built_in,
        }
    }

    /// Creates a printer group initialized with a set of printer names.
    pub fn with_printers(
        id: impl Into<String>,
        name: impl Into<String>,
        printers: impl IntoIterator<Item = String>,
        is_built_in: bool,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            printer_names: printers.into_iter().collect(),
            is_built_in,
        }
    }

    /// Adds a printer to the group. Returns true if newly inserted.
    pub fn add_printer(&mut self, printer_name: String) -> bool {
        self.printer_names.insert(printer_name)
    }

    /// Removes a printer from the group. Returns true if removed.
    pub fn remove_printer(&mut self, printer_name: &str) -> bool {
        self.printer_names.remove(printer_name)
    }

    /// Checks if a printer is in this group.
    pub fn contains(&self, printer_name: &str) -> bool {
        self.printer_names.contains(printer_name)
    }
}

/// Color mode selection for preset configurations.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PresetColorMode {
    #[default]
    Color,
    Monochrome,
}

/// Duplex / two-sided binding style for preset configurations.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PresetDuplex {
    #[default]
    Simplex,
    LongEdge,
    ShortEdge,
}

/// Page orientation rule for preset configurations.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PresetOrientation {
    #[default]
    Auto,
    Portrait,
    Landscape,
}

/// Scaling and sizing algorithm applied to print content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum PresetFitMode {
    #[default]
    FitToPrintable,
    ActualSize100,
    ShrinkOversized,
    CustomScale(f32),
}

/// A saved collection of print options that can be applied in one click.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrintJobPreset {
    pub id: String,
    pub name: String,
    pub description: String,
    pub is_built_in: bool,
    pub copies: u32,
    pub color_mode: PresetColorMode,
    pub duplex: PresetDuplex,
    pub orientation: PresetOrientation,
    pub fit_mode: PresetFitMode,
    pub custom_scale_pct: Option<f32>,
    pub collate: bool,
}

impl PrintJobPreset {
    /// Creates a new job preset with default options.
    pub fn new(id: impl Into<String>, name: impl Into<String>, description: impl Into<String>, is_built_in: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            is_built_in,
            copies: 1,
            color_mode: PresetColorMode::default(),
            duplex: PresetDuplex::default(),
            orientation: PresetOrientation::default(),
            fit_mode: PresetFitMode::default(),
            custom_scale_pct: None,
            collate: true,
        }
    }
}

/// Store for managing printer groups and job presets, with built-in defaults and JSON I/O.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PrintPresetStore {
    pub groups: BTreeMap<String, PrinterGroup>,
    pub presets: BTreeMap<String, PrintJobPreset>,
    pub active_preset_id: Option<String>,
    pub active_group_id: Option<String>,
}

impl PrintPresetStore {
    /// Initializes store with the three standard production presets.
    pub fn with_defaults() -> Self {
        let mut store = Self::default();

        let standard_duplex = PrintJobPreset {
            id: "standard-duplex".to_string(),
            name: "Standard Duplex".to_string(),
            description: "Full color, two-sided long-edge binding, auto-scaled to printable area".to_string(),
            is_built_in: true,
            copies: 1,
            color_mode: PresetColorMode::Color,
            duplex: PresetDuplex::LongEdge,
            orientation: PresetOrientation::Auto,
            fit_mode: PresetFitMode::FitToPrintable,
            custom_scale_pct: None,
            collate: true,
        };

        let monochrome_eco = PrintJobPreset {
            id: "monochrome-eco".to_string(),
            name: "Monochrome Eco".to_string(),
            description: "Grayscale / monochrome K-only, duplex long-edge saving paper & color toner".to_string(),
            is_built_in: true,
            copies: 1,
            color_mode: PresetColorMode::Monochrome,
            duplex: PresetDuplex::LongEdge,
            orientation: PresetOrientation::Auto,
            fit_mode: PresetFitMode::FitToPrintable,
            custom_scale_pct: None,
            collate: true,
        };

        let engineering_100 = PrintJobPreset {
            id: "engineering-100".to_string(),
            name: "Engineering 1:1 Actual Size".to_string(),
            description: "Unscaled 100% exact dimensions for architectural and engineering blueprints".to_string(),
            is_built_in: true,
            copies: 1,
            color_mode: PresetColorMode::Color,
            duplex: PresetDuplex::Simplex,
            orientation: PresetOrientation::Auto,
            fit_mode: PresetFitMode::ActualSize100,
            custom_scale_pct: None,
            collate: true,
        };

        store.upsert_preset(standard_duplex);
        store.upsert_preset(monochrome_eco);
        store.upsert_preset(engineering_100);
        store.active_preset_id = Some("standard-duplex".to_string());

        store
    }

    /// Inserts or updates a preset.
    pub fn upsert_preset(&mut self, preset: PrintJobPreset) {
        self.presets.insert(preset.id.clone(), preset);
    }

    /// Deletes a user preset. Built-in presets cannot be deleted.
    /// Returns true if deleted, false if not found or built-in.
    pub fn delete_preset(&mut self, id: &str) -> bool {
        if let Some(preset) = self.presets.get(id) {
            if preset.is_built_in {
                return false;
            }
        } else {
            return false;
        }

        let removed = self.presets.remove(id).is_some();
        if removed && self.active_preset_id.as_deref() == Some(id) {
            self.active_preset_id = None;
        }
        removed
    }

    /// Retrieves an immutable reference to a preset.
    pub fn get_preset(&self, id: &str) -> Option<&PrintJobPreset> {
        self.presets.get(id)
    }

    /// Retrieves a mutable reference to a preset.
    pub fn get_preset_mut(&mut self, id: &str) -> Option<&mut PrintJobPreset> {
        self.presets.get_mut(id)
    }

    /// Returns the currently active preset, if set and found.
    pub fn active_preset(&self) -> Option<&PrintJobPreset> {
        self.active_preset_id.as_deref().and_then(|id| self.presets.get(id))
    }

    /// Sets the active preset ID.
    pub fn set_active_preset(&mut self, id: Option<String>) {
        self.active_preset_id = id;
    }

    /// Inserts or updates a printer group.
    pub fn upsert_group(&mut self, group: PrinterGroup) {
        self.groups.insert(group.id.clone(), group);
    }

    /// Deletes a printer group. Built-in groups cannot be deleted.
    /// Returns true if deleted, false if not found or built-in.
    pub fn delete_group(&mut self, id: &str) -> bool {
        if let Some(group) = self.groups.get(id) {
            if group.is_built_in {
                return false;
            }
        } else {
            return false;
        }

        let removed = self.groups.remove(id).is_some();
        if removed && self.active_group_id.as_deref() == Some(id) {
            self.active_group_id = None;
        }
        removed
    }

    /// Retrieves an immutable reference to a group.
    pub fn get_group(&self, id: &str) -> Option<&PrinterGroup> {
        self.groups.get(id)
    }

    /// Retrieves a mutable reference to a group.
    pub fn get_group_mut(&mut self, id: &str) -> Option<&mut PrinterGroup> {
        self.groups.get_mut(id)
    }

    /// Returns the currently active group, if set and found.
    pub fn active_group(&self) -> Option<&PrinterGroup> {
        self.active_group_id.as_deref().and_then(|id| self.groups.get(id))
    }

    /// Sets the active group ID.
    pub fn set_active_group(&mut self, id: Option<String>) {
        self.active_group_id = id;
    }

    /// Returns all presets ordered by ID.
    pub fn list_presets(&self) -> Vec<&PrintJobPreset> {
        self.presets.values().collect()
    }

    /// Returns all groups ordered by ID.
    pub fn list_groups(&self) -> Vec<&PrinterGroup> {
        self.groups.values().collect()
    }

    /// Serializes entire store to indented JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserializes store from JSON string.
    pub fn from_json(json_str: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_str)
    }

    /// Exports store to a file on disk.
    pub fn export_to_file(&self, path: &Path) -> std::io::Result<()> {
        let json_str = self.to_json().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut file = File::create(path)?;
        file.write_all(json_str.as_bytes())?;
        file.flush()
    }

    /// Imports store from a file on disk.
    pub fn import_from_file(path: &Path) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Self::from_json(&contents).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_with_defaults() {
        let store = PrintPresetStore::with_defaults();
        assert_eq!(store.presets.len(), 3);
        assert!(store.presets.contains_key("standard-duplex"));
        assert!(store.presets.contains_key("monochrome-eco"));
        assert!(store.presets.contains_key("engineering-100"));
        assert_eq!(store.active_preset_id.as_deref(), Some("standard-duplex"));

        let standard = store.get_preset("standard-duplex").expect("must exist");
        assert!(standard.is_built_in);
        assert_eq!(standard.color_mode, PresetColorMode::Color);
        assert_eq!(standard.duplex, PresetDuplex::LongEdge);
        assert_eq!(standard.fit_mode, PresetFitMode::FitToPrintable);

        let eco = store.get_preset("monochrome-eco").expect("must exist");
        assert_eq!(eco.color_mode, PresetColorMode::Monochrome);

        let eng = store.get_preset("engineering-100").expect("must exist");
        assert_eq!(eng.fit_mode, PresetFitMode::ActualSize100);
    }

    #[test]
    fn test_cannot_delete_built_in_preset() {
        let mut store = PrintPresetStore::with_defaults();
        let deleted = store.delete_preset("standard-duplex");
        assert!(!deleted, "built-in preset deletion must be rejected");
        assert!(store.presets.contains_key("standard-duplex"));
    }

    #[test]
    fn test_custom_preset_lifecycle() {
        let mut store = PrintPresetStore::with_defaults();
        let custom = PrintJobPreset {
            id: "custom-2up".to_string(),
            name: "2-Up Booklet Test".to_string(),
            description: "Custom user preset".to_string(),
            is_built_in: false,
            copies: 2,
            color_mode: PresetColorMode::Monochrome,
            duplex: PresetDuplex::ShortEdge,
            orientation: PresetOrientation::Landscape,
            fit_mode: PresetFitMode::CustomScale(95.0),
            custom_scale_pct: Some(95.0),
            collate: false,
        };

        store.upsert_preset(custom);
        assert_eq!(store.presets.len(), 4);
        store.set_active_preset(Some("custom-2up".to_string()));
        assert_eq!(store.active_preset().map(|p| p.name.as_str()), Some("2-Up Booklet Test"));

        let deleted = store.delete_preset("custom-2up");
        assert!(deleted);
        assert_eq!(store.presets.len(), 3);
        assert!(store.active_preset().is_none());
    }

    #[test]
    fn test_printer_group_lifecycle() {
        let mut store = PrintPresetStore::with_defaults();
        let mut group = PrinterGroup::new("floor-2-cluster", "2nd Floor Copiers", false);
        group.add_printer("HP-LaserJet-M608".to_string());
        group.add_printer("Canon-imageRUNNER-C5535".to_string());

        store.upsert_group(group);
        assert_eq!(store.groups.len(), 1);

        let retrieved = store.get_group("floor-2-cluster").expect("must exist");
        assert_eq!(retrieved.printer_names.len(), 2);
        assert!(retrieved.contains("HP-LaserJet-M608"));
        assert!(retrieved.contains("Canon-imageRUNNER-C5535"));
        assert!(!retrieved.contains("Non-Existent"));

        store.set_active_group(Some("floor-2-cluster".to_string()));
        assert_eq!(store.active_group().map(|g| g.name.as_str()), Some("2nd Floor Copiers"));

        let deleted = store.delete_group("floor-2-cluster");
        assert!(deleted);
        assert_eq!(store.groups.len(), 0);
        assert!(store.active_group().is_none());
    }

    #[test]
    fn test_json_roundtrip_store() {
        let mut store = PrintPresetStore::with_defaults();
        let mut group = PrinterGroup::new("shipping-thermal", "Shipping Thermal Cluster", false);
        group.add_printer("Zebra-ZD420".to_string());
        store.upsert_group(group);

        let json = store.to_json().expect("serialization must succeed");
        let decoded = PrintPresetStore::from_json(&json).expect("deserialization must succeed");

        assert_eq!(store, decoded);
    }
}
