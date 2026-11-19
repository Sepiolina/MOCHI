//! Passphrase entry (spec Annex B.2.10 D20 item 11).
//!
//! * An interactive prompt without echo (`rpassword`), asked twice when a
//!   passphrase is **created**.
//! * `--passphrase-file PATH`: the first line, line ending removed, read once.
//!   A warning on Unix when others can read the file.
//! * `MOCHI_PASSPHRASE`, read **only** behind `--passphrase-env-for-automation`,
//!   whose name says what it is for.
//! * **Never an argument value**: a command line is visible to other users.
//!
//! Secrets are held in [`Passphrase`] and [`Zeroizing`] buffers. Nothing here
//! prints one, and no error message contains one.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use mochi_core::{ErrorCode, MochiError, Result};
use mochi_format::secret::Passphrase;
use zeroize::Zeroizing;

/// The environment variable the automation switch reads.
pub const ENV_VAR: &str = "MOCHI_PASSPHRASE";

/// Where passphrases may come from, from the global options.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    /// `--passphrase-file`, in order; each file holds one passphrase.
    pub files: Vec<PathBuf>,
    /// `--passphrase-env-for-automation`.
    pub env: bool,
}

impl Sources {
    /// Whether the user named a non-interactive source.
    pub fn explicit(&self) -> bool {
        !self.files.is_empty() || self.env
    }
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, msg)
}

fn passphrase(text: &str) -> Result<Passphrase> {
    Passphrase::new(text).map_err(MochiError::from)
}

/// The first line of `bytes`, without its `\n` or `\r\n`.
fn first_line(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(bytes.len());
    let line = &bytes[..end];
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// One passphrase from a file: its first line.
fn from_file(path: &Path, warn: &mut dyn Write) -> Result<Passphrase> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.permissions().mode() & 0o044 != 0 {
                let _ = writeln!(
                    warn,
                    "mochi: warning: {} is readable by other users; restrict it (chmod 600)",
                    path.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    let _ = &warn;
    let bytes = Zeroizing::new(std::fs::read(path).map_err(|e| {
        MochiError::new(
            ErrorCode::IoError,
            format!("--passphrase-file {}: {e}", path.display()),
        )
    })?);
    let line = first_line(&bytes);
    let text = std::str::from_utf8(line).map_err(|_| {
        invalid(format!(
            "--passphrase-file {}: the passphrase is not valid UTF-8",
            path.display()
        ))
    })?;
    passphrase(text)
}

fn from_env() -> Result<Passphrase> {
    let value = Zeroizing::new(std::env::var(ENV_VAR).map_err(|_| {
        invalid(format!(
            "--passphrase-env-for-automation: {ENV_VAR} is not set (or is not valid Unicode)"
        ))
    })?);
    passphrase(&value)
}

/// Passphrases from the explicit sources: every file in order, then the
/// environment variable when allowed.
pub fn explicit(sources: &Sources, warn: &mut dyn Write) -> Result<Vec<Passphrase>> {
    let mut out = Vec::new();
    for f in &sources.files {
        out.push(from_file(f, warn)?);
    }
    if sources.env {
        out.push(from_env()?);
    }
    Ok(out)
}

/// Whether a prompt can be shown: standard input is a terminal.
pub fn can_prompt() -> bool {
    std::io::stdin().is_terminal()
}

fn prompt(label: &str, err: &mut dyn Write) -> Result<Zeroizing<String>> {
    let _ = write!(err, "{label}");
    let _ = err.flush();
    rpassword::read_password()
        .map(Zeroizing::new)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("reading the passphrase: {e}")))
}

/// Ask for an existing archive's passphrase, once, without echo.
pub fn ask(archive: &Path, err: &mut dyn Write) -> Result<Passphrase> {
    let text = prompt(&format!("Passphrase for {}: ", archive.display()), err)?;
    passphrase(&text)
}

/// Ask for a **new** passphrase, twice; the two must agree.
pub fn ask_new(label: &str, err: &mut dyn Write) -> Result<Passphrase> {
    let first = prompt(&format!("New passphrase for {label}: "), err)?;
    let second = prompt("Repeat the passphrase: ", err)?;
    if *first != *second {
        return Err(invalid("the two passphrases differ; nothing was done"));
    }
    passphrase(&first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_loses_its_line_ending_and_nothing_else() {
        assert_eq!(first_line(b"abc\n"), b"abc");
        assert_eq!(first_line(b"abc\r\nsecond"), b"abc");
        assert_eq!(first_line(b"abc"), b"abc");
        assert_eq!(first_line(b" a b \n"), b" a b ");
        assert_eq!(first_line(b""), b"");
        assert_eq!(first_line(b"\n"), b"");
    }

    #[test]
    fn an_empty_file_is_refused_and_the_message_names_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p");
        std::fs::write(&f, "\n").unwrap();
        let e = from_file(&f, &mut Vec::new()).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_passphrase_file_is_read_by_its_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p");
        std::fs::write(&f, "hunter two\r\nignored\n").unwrap();
        let p = from_file(&f, &mut Vec::new()).unwrap();
        assert!(p.matches(b"hunter two"));
    }
}
