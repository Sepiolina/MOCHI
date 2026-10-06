//! [`OsRestoreDir`] on targets without a race-resistant implementation
//! (anything but Linux and Windows, which MOCHI 1.0 does not support, plan
//! O11): refused with `UNSUPPORTED_FEATURE` before anything is written.

use super::*;

/// Why restoring on this platform is refused.
const UNSUPPORTED_PLATFORM: &str = "race-resistant restoration to the filesystem is not \
     available on this platform in this build (plan C6). Nothing was written";

/// A restore destination that can never be opened on this platform.
#[derive(Debug)]
pub struct OsRestoreDir {
    never: std::convert::Infallible,
}

impl OsRestoreDir {
    /// Always `UNSUPPORTED_FEATURE`.
    pub fn open(_path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Err(StorageError::Unsupported(UNSUPPORTED_PLATFORM))
    }

    /// Unreachable: no value of this type exists.
    pub fn path(&self) -> &Path {
        match self.never {}
    }
}

impl RestoreDir for OsRestoreDir {
    type File = OsStorage;
    fn name_issue(&self, _: &[u8]) -> Option<NameIssue> {
        match self.never {}
    }
    fn create_dir(&mut self, _: &[u8]) -> Result<Self, StorageError> {
        match self.never {}
    }
    fn create_file(&mut self, _: &[u8]) -> Result<OsStorage, StorageError> {
        match self.never {}
    }
    fn publish_no_replace(&mut self, _: &[u8], _: &[u8]) -> Result<(), StorageError> {
        match self.never {}
    }
    fn discard(&mut self, _: OsStorage, _: &[u8]) -> Result<(), StorageError> {
        match self.never {}
    }
    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        match self.never {}
    }
    fn case_behavior(&mut self) -> Result<crate::storage::CaseBehavior, StorageError> {
        match self.never {}
    }
    fn entry_exists(&mut self, _: &[u8]) -> Result<bool, StorageError> {
        match self.never {}
    }
    fn apply_attributes(&mut self, _: &[u8], _: EntryKind, _: &Attributes) -> Vec<AttributeIssue> {
        match self.never {}
    }
}
