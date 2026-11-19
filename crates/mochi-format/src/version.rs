//! Version identifiers (spec §1.2). Section 26 requires these to stay distinct.

/// Specification revision this code was written against.
pub const SPEC_REVISION: &str = "2.0";

/// Container wire generation: 2 (`MOCHI2`). Ratified with the 1.0 format
/// (spec §1.2, Annex B D1; descriptor key 3 = null, B.2.14).
pub const WIRE_GENERATION: u32 = 2;

/// Description of the support level (spec §28). The interoperability gates of
/// §28 have passed and the format is ratified at 1.0 (Annex B.2.14); the
/// Preservation gates have not, and no build claims preservation readiness.
pub const FORMAT_STATUS: &str = "1.0 (ratified format)";
