//! Helper-side engine: detect, then list/test/read through the format modules.
//! Everything here runs inside the helper process only; `mochi-core` and
//! `mochi-format` never link this module (checked by `ci/check-invariants.sh`).
//!
//! It is generic over `Read + Seek` so the fuzz targets and tests drive the
//! very code the helper runs, on in-memory bytes.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::detect::detect;
use crate::protocol::{Done, Message, Request};
use crate::tarfmt;
use crate::types::*;
use crate::volumes::SplitName;
use crate::zipfmt;
use crate::{rarfmt, sevenfmt};

/// Where the bytes handed to [`run`] came from, for the formats that need
/// more than a byte stream.
#[derive(Clone, Copy)]
pub struct Origin<'a> {
    /// The archive's path. RAR is read by UnRAR, which opens files itself and
    /// finds the next volume by name; without a path RAR is unavailable (the
    /// in-memory fuzz harness uses a temporary file instead).
    pub path: Option<&'a Path>,
    /// The file name's `.7z.NNN` form, when it has one.
    pub split: SplitName,
    /// Volumes read as one stream (a 7z set); 1 otherwise.
    pub volumes: usize,
}

impl Origin<'_> {
    /// A plain, single stream with no path (tests, in-memory fuzzing).
    pub const MEMORY: Origin<'static> = Origin {
        path: None,
        split: SplitName::NotSplit,
        volumes: 1,
    };
}

/// Where the engine sends messages. An `Err` means the consumer is gone.
pub type Out<'a> = dyn FnMut(Message) -> io::Result<()> + 'a;

pub enum EngineError {
    /// The archive could not be processed; becomes an `Error` message.
    Archive(ForeignError),
    /// The output channel failed; nothing more can be reported.
    Sink(io::Error),
}

impl From<io::Error> for EngineError {
    fn from(e: io::Error) -> Self {
        EngineError::Sink(e)
    }
}

impl From<ForeignError> for EngineError {
    fn from(e: ForeignError) -> Self {
        EngineError::Archive(e)
    }
}

/// How an operation ended, for the `Done` message.
#[derive(Default)]
pub struct Tally {
    pub entries: u64,
    pub bytes_out: u64,
    pub complete: bool,
    pub stop_reason: Option<String>,
}

pub fn run<R: Read + Seek>(
    mut src: R,
    origin: &Origin<'_>,
    req: &Request,
    out: &mut Out,
) -> Result<(), EngineError> {
    let len = src.seek(SeekFrom::End(0)).map_err(|e| {
        ForeignError::new(ErrorCode::OpenFailed, format!("cannot read archive: {e}"))
    })?;
    let format = detect(&mut src)?;
    src.seek(SeekFrom::Start(0)).map_err(|e| {
        ForeignError::new(ErrorCode::OpenFailed, format!("cannot read archive: {e}"))
    })?;
    out(Message::Format(format))?;
    let tally = match format {
        ForeignFormat::Zip => zipfmt::run(src, len, req, out)?,
        ForeignFormat::Tar => tarfmt::run(src, len, tarfmt::Layer::None, req, out)?,
        ForeignFormat::TarGz => tarfmt::run(src, len, tarfmt::Layer::Gzip, req, out)?,
        ForeignFormat::TarZst => tarfmt::run(src, len, tarfmt::Layer::Zstd, req, out)?,
        ForeignFormat::SevenZ => sevenfmt::run(src, len, origin, req, out)?,
        ForeignFormat::Rar => rarfmt::run(origin, req, out)?,
    };
    out(Message::Done(Done {
        complete: tally.complete,
        entries: tally.entries,
        bytes_out: tally.bytes_out,
        stop_reason: tally.stop_reason,
    }))?;
    Ok(())
}

pub(crate) fn emit_breach(
    out: &mut Out,
    kind: BreachKind,
    entry: Option<u64>,
    limit: u64,
    observed: u64,
) -> io::Result<Breach> {
    let b = Breach {
        kind,
        entry,
        limit,
        observed,
    };
    out(Message::Breach(b.clone()))?;
    Ok(b)
}

/// The compression-ratio test shared by both formats: enough output to matter,
/// and more than `ratio` times the compressed bytes behind it.
pub(crate) fn ratio_exceeded(limits: &Limits, produced: u64, compressed: u64) -> bool {
    produced > limits.ratio_min_output
        && produced > limits.max_ratio.saturating_mul(compressed.max(1))
}

pub(crate) fn truncate_text(mut s: String) -> String {
    let max = crate::protocol::MAX_MESSAGE_TEXT;
    if s.len() > max {
        let mut i = max;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        s.truncate(i);
    }
    s
}
