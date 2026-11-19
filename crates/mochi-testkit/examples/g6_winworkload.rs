//! Gate G6 Windows-durability helper (spec §12.2, D13; gate G6). Runs inside
//! the Windows guest of the block-level crash-replay harness
//! (`ci/g6-windows-crashreplay.sh`, `docs/dev/windows-crash-replay.md`).
//!
//! ```text
//! g6_winworkload workload <dir> <port|none> [--no-dir-sync]  # create archives, append commits, ack each
//! g6_winworkload check <dir> <acks-file>      # judge what a replayed disk holds
//! ```
//!
//! **Workload.** For each of [`ARCHIVES`] archives it runs the real storage
//! path: `ArchiveWriter::create_in` over `OsDir` (D13 creation: temporary
//! file, sync, publication, directory flush), then one `commit` per further
//! step of [`scripted_history`]. After every acknowledgement it writes
//! `MARKREQ ack:<name>:<seq>:<D|U>` to the host over `port` (a serial port,
//! never the data disk) and blocks until the host answers `MARKED ...`. The
//! host has then inserted a mark into the `dm-log-writes` log, so the log
//! orders the acknowledgement after every write the guest had completed.
//! `D` is [`PublishDurability::Durable`], `U` is `DirectoryUnconfirmed`.
//!
//! **Judge** (`check`), the G7 rules extended to appends. The acks file holds
//! one `<name> <seq> <D|U>` line per acknowledgement the host had marked
//! before the replay point. For every archive:
//!
//! * the final name holds nothing, or an archive that opens and whose state
//!   is exactly the state after some scripted commit (never a mixed commit);
//! * if present, its head is at least the highest `Durable` acknowledgement;
//! * a `Durable` creation acknowledgement means the name must be present;
//! * a name that is absent although only `DirectoryUnconfirmed` was
//!   acknowledged is *allowed* (the acknowledgement said so) and counted as
//!   a soft observation, as is an absent name after a durable append whose
//!   creation was unconfirmed;
//! * leftover temporary files are removable (nobody holds their lock);
//! * nothing else is in the directory (the control experiments' `ctl`
//!   directory and the volume's own `$`-entries aside).
//!
//! One JSON line is printed; the exit status is 0 on pass and 1 on fail.
//! `--no-dir-sync` is a diagnostic variant: the directory is wrapped so that
//! `sync_directory` flushes nothing and reports `Unconfirmed`, everything else
//! being the real `OsDir`. It isolates what the publication primitive alone
//! (rename, or `MoveFileExW` write-through) makes durable, without the
//! best-effort directory flush that normally follows it.
//!
//! With `port` = `none` the workload prints its acknowledgements and does
//! not wait, for running it outside the VM.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

use mochi_core::publish::{open_head, ArchiveWriter, PublishDurability, ReadOptions};
use mochi_core::storage::os::{OsDir, OsReadStorage, OsStorage};
use mochi_core::storage::{
    DirectoryDurability, ReadStorage, RemoveOutcome, Storage, StorageDir, StorageError,
};
use mochi_testkit::archive::{read_state, scripted_history, test_options, Job};
use mochi_testkit::SeqIds;
use serde_json::json;

const ARCHIVES: u64 = 4;

fn name_of(i: u64) -> String {
    format!("a{i}.mochi")
}

fn is_temp(name: &str, archive: &str) -> bool {
    name.starts_with(&format!(".{archive}.")) && name.ends_with(".mochi-tmp")
}

/// The host side of the acknowledgement channel.
struct Port(Option<std::fs::File>);

impl Port {
    fn open(spec: &str) -> Self {
        if spec == "none" {
            return Self(None);
        }
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(spec)
            .unwrap_or_else(|e| panic!("cannot open {spec}: {e}"));
        Self(Some(f))
    }

    /// Ask the host to mark the log, and wait until it has.
    fn mark(&mut self, label: &str) {
        let Some(f) = self.0.as_mut() else {
            println!("MARKREQ {label}");
            return;
        };
        f.write_all(format!("MARKREQ {label}\n").as_bytes())
            .unwrap();
        f.flush().unwrap();
        // One byte at a time: a serial read with no timeouts configured
        // waits for the whole buffer.
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            let n = f.read(&mut b).unwrap();
            if n == 0 {
                continue;
            }
            if b[0] == b'\n' {
                break;
            }
            if b[0] != b'\r' {
                line.push(b[0]);
            }
        }
        let line = String::from_utf8_lossy(&line);
        assert_eq!(
            line,
            format!("MARKED {label}"),
            "unexpected reply from the host"
        );
    }
}

fn code(d: &PublishDurability) -> &'static str {
    match d {
        PublishDurability::Durable => "D",
        PublishDurability::DirectoryUnconfirmed(_) => "U",
    }
}

/// A file whose directory flush does nothing (the `--no-dir-sync` variant).
struct NoDirSyncFile(OsStorage);

impl ReadStorage for NoDirSyncFile {
    fn size(&self) -> Result<u64, StorageError> {
        self.0.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        self.0.read_at(offset, buf)
    }
}

const SUPPRESSED: &str = "directory flush suppressed by the harness (--no-dir-sync)";

impl Storage for NoDirSyncFile {
    fn append(&mut self, data: &[u8]) -> Result<u64, StorageError> {
        self.0.append(data)
    }
    fn sync_data(&mut self) -> Result<(), StorageError> {
        self.0.sync_data()
    }
    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        Ok(DirectoryDurability::Unconfirmed(SUPPRESSED.into()))
    }
    fn try_lock_exclusive(&mut self) -> Result<(), StorageError> {
        self.0.try_lock_exclusive()
    }
    fn unlock(&mut self) -> Result<(), StorageError> {
        self.0.unlock()
    }
    fn truncate(&mut self, new_len: u64) -> Result<(), StorageError> {
        self.0.truncate(new_len)
    }
}

/// The real `OsDir`, except that `sync_directory` flushes nothing.
struct NoDirSyncDir(OsDir);

impl StorageDir for NoDirSyncDir {
    type File = NoDirSyncFile;
    fn open(&mut self, name: &str) -> Result<NoDirSyncFile, StorageError> {
        self.0.open(name).map(NoDirSyncFile)
    }
    fn create_exclusive(&mut self, name: &str) -> Result<NoDirSyncFile, StorageError> {
        self.0.create_exclusive(name).map(NoDirSyncFile)
    }
    fn publish_no_replace(&mut self, from: &str, to: &str) -> Result<(), StorageError> {
        self.0.publish_no_replace(from, to)
    }
    fn publish_archive(
        &mut self,
        file: &mut NoDirSyncFile,
        from: &str,
        to: &str,
    ) -> Result<(), StorageError> {
        self.0.publish_archive(&mut file.0, from, to)
    }
    fn discard(&mut self, file: NoDirSyncFile, name: &str) -> Result<(), StorageError> {
        self.0.discard(file.0, name)
    }
    fn remove_if_unlocked(&mut self, name: &str) -> Result<RemoveOutcome, StorageError> {
        self.0.remove_if_unlocked(name)
    }
    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        Ok(DirectoryDurability::Unconfirmed(SUPPRESSED.into()))
    }
}

/// What the workload needs of a directory, so one body serves both variants.
trait Publisher {
    fn mechanism(&self) -> String;
}

impl Publisher for OsDir {
    fn mechanism(&self) -> String {
        format!("{:?}", self.last_publish())
    }
}

impl Publisher for NoDirSyncDir {
    fn mechanism(&self) -> String {
        format!("{:?}", self.0.last_publish())
    }
}

fn workload(dir: &Path, port: &str, no_dir_sync: bool) {
    let port = Port::open(port);
    if no_dir_sync {
        run_workload(NoDirSyncDir(OsDir::open(dir).unwrap()), port);
    } else {
        run_workload(OsDir::open(dir).unwrap(), port);
    }
}

fn run_workload<D: StorageDir + Publisher>(mut d: D, mut port: Port)
where
    D::File: Storage,
{
    let history = scripted_history();
    let job = Job::new();
    for i in 0..ARCHIVES {
        let name = name_of(i);
        let (mut w, outcome) = ArchiveWriter::create_in(
            &mut d,
            &name,
            Box::new(SeqIds::new(100 + i)),
            test_options(),
            history[0].tx.clone(),
            &job.ctx(),
        )
        .unwrap_or_else(|e| panic!("create {name}: {e}"));
        assert_eq!(outcome.seq, 0);
        // Evidence of what was exercised: the mechanism of the publication.
        println!("publish mechanism {name}: {}", d.mechanism());
        port.mark(&format!("ack:{name}:0:{}", code(&outcome.durability)));
        for (seq, step) in history.iter().enumerate().skip(1) {
            let outcome = w
                .commit(step.tx.clone(), &job.ctx())
                .unwrap_or_else(|e| panic!("commit {seq} of {name}: {e}"));
            assert_eq!(outcome.seq, seq as u64);
            port.mark(&format!("ack:{name}:{seq}:{}", code(&outcome.durability)));
        }
        drop(w);
    }
    println!(
        "workload done: {ARCHIVES} archives, {} commits each",
        history.len()
    );
}

#[derive(Default)]
struct Acks {
    /// (seq, durable) in acknowledgement order.
    list: Vec<(u64, bool)>,
}

fn parse_acks(path: &Path) -> BTreeMap<String, Acks> {
    let mut out: BTreeMap<String, Acks> = BTreeMap::new();
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(' ').collect();
        assert_eq!(f.len(), 3, "bad acks line {line:?}");
        let seq: u64 = f[1].parse().unwrap();
        out.entry(f[0].to_string())
            .or_default()
            .list
            .push((seq, f[2] == "D"));
    }
    out
}

fn check(dir: &Path, acks_file: &Path) -> bool {
    let acks = parse_acks(acks_file);
    let history = scripted_history();
    let mut hard: Vec<String> = Vec::new();
    let mut soft: Vec<String> = Vec::new();

    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('$') && n != "System Volume Information" && n != "ctl")
        .collect();
    let mut known = Vec::new();
    for i in 0..ARCHIVES {
        let n = name_of(i);
        known.push(n);
    }
    for n in &names {
        let ours = known.contains(n) || (0..ARCHIVES).any(|i| is_temp(n, &name_of(i)));
        if !ours {
            hard.push(format!("unexpected entry {n:?}"));
        }
    }

    let mut per_archive = Vec::new();
    for i in 0..ARCHIVES {
        let name = name_of(i);
        let mine = acks.get(&name);
        let created_durable = mine.is_some_and(|a| a.list.contains(&(0, true)));
        let any_ack = mine.is_some_and(|a| !a.list.is_empty());
        let highest_durable =
            mine.and_then(|a| a.list.iter().filter(|(_, d)| *d).map(|(s, _)| *s).max());
        let highest_ack = mine.and_then(|a| a.list.iter().map(|(s, _)| *s).max());
        let present = names.contains(&name);
        let verdict;
        if present {
            let file = OsReadStorage::open(dir.join(&name)).unwrap();
            match open_head(&file, &ReadOptions::default()) {
                Err(e) => {
                    hard.push(format!("{name} exists but does not open: {e}"));
                    verdict = "unopenable".to_string();
                }
                Ok(head) => {
                    let seq = head.seq();
                    match history.get(seq as usize) {
                        None => {
                            hard.push(format!("{name} head seq {seq} was never committed"));
                            verdict = format!("bad-seq-{seq}");
                        }
                        Some(step) => {
                            match read_state(&file, &head) {
                                Ok(state) if state == step.after => {}
                                Ok(_) => hard.push(format!(
                                    "{name} at seq {seq} holds a state no commit produced"
                                )),
                                Err(e) => {
                                    hard.push(format!("{name} at seq {seq} does not read: {e}"))
                                }
                            }
                            if let Some(h) = highest_durable {
                                if seq < h {
                                    hard.push(format!(
                                        "{name}: durable commit {h} acknowledged, head is {seq}"
                                    ));
                                }
                            }
                            verdict = format!("seq-{seq}");
                        }
                    }
                }
            }
        } else {
            if created_durable {
                hard.push(format!(
                    "{name}: durable creation acknowledged, name is gone"
                ));
            } else if any_ack {
                let extra = if highest_durable.is_some() {
                    " (a later append was acknowledged durable)"
                } else {
                    ""
                };
                soft.push(format!(
                    "{name}: absent after acknowledged seq {}; creation was DirectoryUnconfirmed{extra}",
                    highest_ack.unwrap_or(0)
                ));
            }
            verdict = "absent".to_string();
        }
        per_archive.push(json!({
            "archive": name,
            "verdict": verdict,
            "created_durable": created_durable,
            "highest_acknowledged": highest_ack,
            "highest_durable": highest_durable,
        }));
    }

    // Nobody holds a leftover temporary file's lock: cleanup removes it.
    let mut d = OsDir::open(dir).unwrap();
    let mut removed = 0;
    for n in names
        .iter()
        .filter(|n| (0..ARCHIVES).any(|i| is_temp(n, &name_of(i))))
    {
        match d.remove_if_unlocked(n) {
            Ok(RemoveOutcome::Removed) => removed += 1,
            other => hard.push(format!("temporary file {n} not removable: {other:?}")),
        }
    }

    let pass = hard.is_empty();
    println!(
        "{}",
        json!({
            "result": if pass { "pass" } else { "fail" },
            "hard": hard,
            "soft": soft,
            "temporary_files_removed": removed,
            "archives": per_archive,
        })
    );
    pass
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: g6_winworkload workload <dir> <port|none> [--no-dir-sync] | check <dir> <acks-file>";
    let dir = Path::new(args.get(2).expect(usage));
    match args.get(1).map(String::as_str) {
        Some("workload") => workload(
            dir,
            args.get(3).expect(usage),
            args.get(4).map(String::as_str) == Some("--no-dir-sync"),
        ),
        Some("check") => {
            if !check(dir, Path::new(args.get(3).expect(usage))) {
                std::process::exit(1);
            }
        }
        _ => panic!("{usage}"),
    }
}
