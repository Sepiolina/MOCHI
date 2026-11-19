//! A streaming reader over a sequence of Zstandard frames, on the pure-Rust
//! `ruzstd` decoder (MIT). Runs inside the helper only.
//!
//! `ruzstd`'s streaming adapter handles exactly one frame and does not compare
//! the frame checksum, so this wrapper drives its frame decoder itself:
//!
//! - frames follow one another until the input ends (what `zstd` writes when
//!   files are concatenated, and what a parallel writer produces);
//! - skippable frames (magic `0x184D2A5?`) are skipped by their stated length;
//! - when a frame declares a content checksum it is compared at the frame's
//!   end, and a mismatch is an I/O error, never a silent pass;
//! - the window is capped at ruzstd's default (128 MiB), so a hostile header
//!   cannot ask for more memory than that.
//!
//! Every failure surfaces as a plain `io::Error`; the tar layer above turns it
//! into a reported stream failure.

use std::io::{self, BufRead, BufReader, Read};

use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

/// The little-endian magic of a Zstandard frame, as the bytes appear in a file.
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Decompresses `src` frame by frame.
pub struct ZstdReader<R: Read> {
    src: BufReader<R>,
    dec: FrameDecoder,
    /// A frame is open: its blocks are being decoded or drained.
    active: bool,
    /// The input is exhausted and every frame was closed.
    done: bool,
    /// Frames opened (skippable frames excluded).
    frames: u64,
    /// Frames that declared no content checksum, so nothing vouches for them.
    unchecked: u64,
}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// A decoder error as an I/O error. A window larger than the cap is a policy
/// refusal (`Unsupported`); everything else is damage (`InvalidData`).
fn decode_err(e: FrameDecoderError) -> io::Error {
    let kind = match e {
        FrameDecoderError::WindowSizeTooBig { .. } => io::ErrorKind::Unsupported,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, format!("zstd: {e}"))
}

impl<R: Read> ZstdReader<R> {
    pub fn new(src: R) -> Self {
        ZstdReader {
            src: BufReader::with_capacity(64 << 10, src),
            dec: FrameDecoder::new(),
            active: false,
            done: false,
            frames: 0,
            unchecked: 0,
        }
    }

    /// Whether the stream ended cleanly with at least one frame and every
    /// frame carried a checksum that matched (a mismatch never gets here: it
    /// is an error when the frame closes).
    pub fn fully_checksummed(&self) -> bool {
        self.done && self.frames > 0 && self.unchecked == 0
    }

    /// Skips `n` bytes of a skippable frame.
    fn skip(&mut self, n: u32) -> io::Result<()> {
        let want = u64::from(n);
        let got = io::copy(&mut (&mut self.src).take(want), &mut io::sink())?;
        if got < want {
            return Err(other("a skippable zstd frame is cut short"));
        }
        Ok(())
    }

    /// Opens the next frame, or marks the stream done when the input ends at a
    /// frame boundary.
    fn open_next(&mut self) -> io::Result<()> {
        loop {
            if self.src.fill_buf()?.is_empty() {
                self.done = true;
                return Ok(());
            }
            match self.dec.init(&mut self.src) {
                Ok(()) => {
                    self.active = true;
                    self.frames += 1;
                    return Ok(());
                }
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => self.skip(length)?,
                Err(e) => return Err(decode_err(e)),
            }
        }
    }

    /// The open frame has been fully drained: compare its checksum.
    fn close_frame(&mut self) -> io::Result<()> {
        self.active = false;
        match self.dec.get_checksum_from_data() {
            Some(stored) => match self.dec.get_calculated_checksum() {
                Some(computed) if computed == stored => Ok(()),
                Some(computed) => Err(other(format!(
                    "zstd frame checksum mismatch (stored {stored:08x}, computed {computed:08x})"
                ))),
                None => Err(other("zstd frame checksum could not be computed")),
            },
            None => {
                self.unchecked += 1;
                Ok(())
            }
        }
    }
}

impl<R: Read> Read for ZstdReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.done {
                return Ok(0);
            }
            if !self.active {
                self.open_next()?;
                continue;
            }
            while self.dec.can_collect() == 0 && !self.dec.is_finished() {
                self.dec
                    .decode_blocks(&mut self.src, BlockDecodingStrategy::UptoBlocks(1))
                    .map_err(decode_err)?;
            }
            if self.dec.can_collect() > 0 {
                let n = self.dec.read(buf)?;
                if n == 0 {
                    return Err(other("zstd decoder made no progress"));
                }
                return Ok(n);
            }
            if self.dec.is_finished() && self.dec.can_collect() == 0 {
                self.close_frame()?;
            }
        }
    }
}
