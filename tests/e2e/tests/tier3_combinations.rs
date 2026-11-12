//! Tier 3: Cross-Feature Pairwise Interactions Test Suite (27 tests minimum).
//!
//! Authoritative verification suite exercising orthogonal pairwise parameter combinations
//! across all core architectural axes of the pdfcraft Production Print Studio (R1–R5):
//! - Tests 01–09: Driver Generation × Color Mode × Margin Engine Combinations
//! - Tests 10–18: Duplex Mode × Streaming & Concurrency Combinations
//! - Tests 19–27: Fault Tolerance & Failover × Cross-Axis Interactions

#[path = "tier3/driver_color_margin_combinations.rs"]
mod driver_color_margin_combinations;

#[path = "tier3/duplex_streaming_combinations.rs"]
mod duplex_streaming_combinations;

#[path = "tier3/fault_failover_telemetry_combinations.rs"]
mod fault_failover_telemetry_combinations;
