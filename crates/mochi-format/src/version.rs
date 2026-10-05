//! Version identifiers (spec §1.2). Section 26 requires these to stay distinct.

/// Specification revision this code was written against.
pub const SPEC_REVISION: &str = "2.0";

/// Container wire generation. **Proposed**, not ratified (spec §1.2, Annex B D1).
pub const WIRE_GENERATION: u32 = 2;

/// Mandatory description of support level until the §28 gates pass.
pub const FORMAT_STATUS: &str = "experimental / draft-compatible";
