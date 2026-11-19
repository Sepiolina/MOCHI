//! Building one commit from a source tree (plan C14 `create` / `append`,
//! desktop D2 "Create and add"): the walk, the archive names, and the
//! attributes, in one place for every client (spec §23.3 #1, §23.4).
//!
//! The source is reached through [`SourceTree`], so this module does no
//! I/O itself (AGENTS.md: all I/O in `mochi-core` through traits); the OS
//! implementation is `storage::os::OsSourceTree`.
//!
//! # Rules (delegated decisions, recorded in plan C14)
//!
//! * **Names.** Each input becomes a top-level entry named by its last
//!   component, as `tar` and `zip` do. Children keep their names byte for
//!   byte (Windows names as WTF-8, O24); nothing is normalized, case-folded,
//!   or altered. A name the archive cannot represent is refused, with the
//!   whole import, before anything is written.
//! * **Order.** Parents before children, siblings in name-byte order, so a
//!   commit is deterministic for a given tree.
//! * **Kinds.** Regular files and directories are stored. Symbolic links
//!   are **skipped and reported**: the symlink entry kind is decided (O6)
//!   but not built (manifest kind 2 is reserved; plan C4, C6). Links are
//!   never followed, including an input that is itself a link. Devices,
//!   FIFOs, and sockets are skipped and reported. Hard links become separate
//!   files (O6).
//! * **Attributes** are the promised set (O6), captured by the source tree.
//! * **Failures.** A source entry that cannot be read fails the import:
//!   nothing is committed, rather than a commit that silently lacks a file.
//!   A file is opened during the walk (so an unreadable one fails early) and
//!   read again while the commit is written; a read failure then fails the
//!   commit, which publishes nothing.
//! * **Memory.** Content is not held: each file goes into the transaction
//!   as a [`FileSource`] ([`Transaction::put_file_from`]) and the writer
//!   reads it one chunk at a time, so an import's size is bounded by the
//!   archive's limits, not by memory. [`ImportOptions::max_total_bytes`] is
//!   an optional cap the caller chooses (`LIMIT_EXCEEDED` above it, never
//!   truncated); there is none by default.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::catalog::path::ArchivePath;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::manifest::Attributes;
use crate::publish::{FileSource, Transaction};

/// Progress phase of [`import`]: `completed` is the content bytes found so
/// far (they are read later, by the commit's `content` phase).
pub const IMPORT_PHASE: &str = "import";

/// What a source entry is, without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    File,
    Directory,
    Symlink,
    /// A device, FIFO, socket, or anything else.
    Other,
}

/// One source entry's metadata (not following links).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceMeta {
    pub kind: SourceKind,
    /// Length in bytes for a file; anything for the other kinds.
    pub len: u64,
    pub attributes: Attributes,
}

/// A tree to import from.
pub trait SourceTree {
    /// An opaque handle to one entry (an OS path, a test node).
    type Node: Clone;

    /// The entry's metadata. Never follows a symbolic link.
    fn meta(&self, node: &Self::Node) -> Result<SourceMeta>;

    /// A directory's children: each name as archive bytes (one path
    /// component, WTF-8 on Windows) and its handle. Any order.
    fn children(&self, node: &Self::Node) -> Result<Vec<(Vec<u8>, Self::Node)>>;

    /// A file's content, to be read while the commit is written. `len` is
    /// the length [`SourceTree::meta`] reported. An implementation should
    /// check here that the file can be opened, so that an unreadable file
    /// fails the walk before anything is written.
    fn source(&self, node: &Self::Node, len: u64) -> Result<Arc<dyn FileSource>>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// Most content bytes one import may add, if the caller sets a cap.
    /// `None` (the default): no cap beyond the archive's own limits.
    pub max_total_bytes: Option<u64>,
}

/// An entry that was not imported, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: ArchivePath,
    pub kind: SourceKind,
    pub reason: String,
}

/// The commit an import built, and what it left out.
#[derive(Debug, Clone)]
pub struct Import {
    pub transaction: Transaction,
    pub files: u64,
    pub directories: u64,
    /// Content bytes.
    pub bytes: u64,
    pub skipped: Vec<Skipped>,
}

impl Import {
    /// Every input entry was imported.
    pub fn complete(&self) -> bool {
        self.skipped.is_empty()
    }
}

fn skip_reason(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Symlink => {
            "symbolic links are not stored by this build (plan O6: the entry kind is decided \
             but not built); the link was not followed"
        }
        _ => "only regular files and directories are stored (plan O6)",
    }
}

/// Walk `inputs` (each with the archive path it becomes) and build one
/// transaction (see the module rules). A job: progress per file, and
/// cancellation between entries. Nothing is written anywhere.
pub fn import<T: SourceTree>(
    tree: &T,
    inputs: &[(ArchivePath, T::Node)],
    opts: &ImportOptions,
    ctx: &JobContext<'_>,
) -> Result<Import> {
    import_into(tree, inputs, Transaction::new(), opts, ctx)
}

/// [`import`] appending to `transaction`, whose existing operations (for
/// example deletions) come first in the commit.
pub fn import_into<T: SourceTree>(
    tree: &T,
    inputs: &[(ArchivePath, T::Node)],
    transaction: Transaction,
    opts: &ImportOptions,
    ctx: &JobContext<'_>,
) -> Result<Import> {
    let mut seen = BTreeSet::new();
    for (p, _) in inputs {
        if !seen.insert(p.clone()) {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "two inputs would both become {:?} in the archive; nothing was imported",
                    String::from_utf8_lossy(p.as_stored())
                ),
            ));
        }
    }
    let mut out = Import {
        transaction,
        files: 0,
        directories: 0,
        bytes: 0,
        skipped: Vec::new(),
    };
    // Depth-first, parents first; the stack holds siblings in reverse so
    // they pop in name order.
    let mut stack: Vec<(ArchivePath, T::Node)> = inputs.iter().rev().cloned().collect();
    ctx.report(IMPORT_PHASE, 0, None);
    while let Some((path, node)) = stack.pop() {
        ctx.check_cancelled()?;
        let meta = tree.meta(&node)?;
        match meta.kind {
            SourceKind::File => {
                let bytes = out.bytes.saturating_add(meta.len);
                if let Some(max) = opts.max_total_bytes.filter(|m| bytes > *m) {
                    return Err(too_much(max));
                }
                let source = tree.source(&node, meta.len)?;
                out.bytes = bytes;
                out.files += 1;
                out.transaction.put_file_from(path, source, meta.attributes);
                ctx.report(IMPORT_PHASE, out.bytes, None);
            }
            SourceKind::Directory => {
                out.directories += 1;
                out.transaction.put_dir(path.clone(), meta.attributes);
                let mut children = tree.children(&node)?;
                children.sort_by(|a, b| a.0.cmp(&b.0));
                for (name, child) in children.into_iter().rev() {
                    let mut components: Vec<Vec<u8>> =
                        path.components().map(<[u8]>::to_vec).collect();
                    components.push(name);
                    stack.push((ArchivePath::from_components(components)?, child));
                }
            }
            kind @ (SourceKind::Symlink | SourceKind::Other) => out.skipped.push(Skipped {
                path,
                kind,
                reason: skip_reason(kind).into(),
            }),
        }
    }
    Ok(out)
}

fn too_much(max: u64) -> MochiError {
    MochiError::new(
        ErrorCode::LimitExceeded,
        format!(
            "the input holds more than {max} content bytes, the cap chosen for this import; \
             nothing was imported"
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::job::{CancellationToken, NullProgress};

    /// A tree in memory: node = path string.
    struct Mem(BTreeMap<String, (SourceKind, Vec<u8>)>);

    impl SourceTree for Mem {
        type Node = String;
        fn meta(&self, n: &String) -> Result<SourceMeta> {
            let (kind, data) = self.0.get(n).ok_or_else(|| {
                MochiError::new(ErrorCode::IoError, format!("{n}: no such entry"))
            })?;
            Ok(SourceMeta {
                kind: *kind,
                len: data.len() as u64,
                attributes: Attributes::default(),
            })
        }
        fn children(&self, n: &String) -> Result<Vec<(Vec<u8>, String)>> {
            let prefix = format!("{n}/");
            Ok(self
                .0
                .keys()
                .filter_map(|k| {
                    let rest = k.strip_prefix(&prefix)?;
                    (!rest.contains('/')).then(|| (rest.as_bytes().to_vec(), k.clone()))
                })
                .collect())
        }
        fn source(&self, n: &String, _len: u64) -> Result<Arc<dyn FileSource>> {
            Ok(Arc::new(Bytes(self.0[n].1.clone())))
        }
    }

    struct Bytes(Vec<u8>);

    impl FileSource for Bytes {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn open(&self) -> Result<Box<dyn crate::publish::FileReader + '_>> {
            unreachable!("the walk never reads content")
        }
        fn describe(&self) -> String {
            "memory".into()
        }
    }

    fn tree() -> Mem {
        let mut m = BTreeMap::new();
        m.insert("src".into(), (SourceKind::Directory, vec![]));
        m.insert("src/b.txt".into(), (SourceKind::File, b"bb".to_vec()));
        m.insert("src/a".into(), (SourceKind::Directory, vec![]));
        m.insert("src/a/x".into(), (SourceKind::File, b"xxx".to_vec()));
        m.insert("src/link".into(), (SourceKind::Symlink, vec![]));
        m.insert("src/fifo".into(), (SourceKind::Other, vec![]));
        m.insert("top".into(), (SourceKind::File, b"t".to_vec()));
        Mem(m)
    }

    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }

    fn run(t: &Mem, inputs: &[(&str, &str)], max: Option<u64>) -> Result<Import> {
        let cancel = CancellationToken::new();
        let ctx = JobContext {
            progress: &NullProgress,
            cancel: &cancel,
        };
        let inputs: Vec<(ArchivePath, String)> =
            inputs.iter().map(|(a, n)| (p(a), n.to_string())).collect();
        import(
            t,
            &inputs,
            &ImportOptions {
                max_total_bytes: max,
            },
            &ctx,
        )
    }

    #[test]
    fn walks_parents_first_and_skips_what_it_cannot_store() {
        let i = run(&tree(), &[("src", "src"), ("top", "top")], None).unwrap();
        assert_eq!((i.files, i.directories, i.bytes), (3, 2, 6));
        let skipped: Vec<_> = i
            .skipped
            .iter()
            .map(|s| (s.path.as_stored().to_vec(), s.kind))
            .collect();
        assert_eq!(
            skipped,
            [
                (b"src/fifo".to_vec(), SourceKind::Other),
                (b"src/link".to_vec(), SourceKind::Symlink)
            ]
        );
        assert!(!i.complete());
        assert!(!i.transaction.is_empty());
    }

    #[test]
    fn duplicate_destinations_and_inputs_over_a_chosen_cap_are_refused() {
        let e = run(&tree(), &[("top", "top"), ("top", "src/b.txt")], None).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        let e = run(&tree(), &[("src", "src")], Some(4)).unwrap_err();
        assert_eq!(e.code, ErrorCode::LimitExceeded);
        assert!(run(&tree(), &[("src", "src")], Some(5)).is_ok());
    }

    #[test]
    fn an_unreadable_entry_fails_the_whole_import() {
        let e = run(&tree(), &[("gone", "missing")], None).unwrap_err();
        assert_eq!(e.code, ErrorCode::IoError);
    }
}
