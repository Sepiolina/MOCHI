//! Health from locally recorded evidence (spec §20.3, §20.4, §21, §23.2;
//! plan C14 `health`; next-work-plan K5, delegated 2026-10-08).
//!
//! `mochi health` runs no check. It summarizes what this client recorded when
//! it last ran `verify`, `fsck`, `restore-test`, and `repair apply`, judged
//! against a policy, for one archive's **current head**. The logic lives here,
//! with no I/O, so the desktop app shares it; the CLI keeps the evidence log
//! (`<state dir>/evidence/<archive id>.jsonl`) and reads the archive's head.
//!
//! # Evidence
//!
//! An [`EvidenceRecord`] is one completed run: which command, at what level,
//! when, at which head, the status of each dimension it reported, its exit
//! code, its scope, and the stable codes of its error findings. A dimension
//! whose recorded status is `UNKNOWN` was **not assessed** by that run and is
//! ignored for that dimension (a later structural `verify` must not erase an
//! earlier stored-integrity `PASS` at the same head).
//!
//! # Rules, per dimension (K5)
//!
//! Integrity is assessed by `verify`, `fsck`, and `repair apply` records;
//! recoverability also by `restore-test`. For one *kind* of evidence (the
//! policy's `verify` for the first three, `restore_test` for the last), take
//! the latest record that assessed the dimension:
//!
//! 1. none → `UNKNOWN`;
//! 2. recorded `FAIL` → `FAIL`, **whatever its age and head**: an append-only
//!    archive does not heal the bytes a check found damaged, so a failure
//!    stands until newer evidence of that kind replaces it;
//! 3. recorded at a head other than the current one (commits were added) →
//!    `UNKNOWN`, naming both heads;
//! 4. completed in the future of the clock → `UNKNOWN`;
//! 5. older than the policy's `max_age_days` for that kind → `OVERDUE`
//!    (strictly older: evidence exactly at the limit is still current);
//! 6. otherwise its recorded status.
//!
//! The dimension is the worst ([`Status::rollup`]) over the kinds the policy
//! names with an age limit and that feed it: naming `restore_test` *requires*
//! restore evidence. A kind the policy does not name is not required and has
//! no age limit, **but a failure it recorded still stands** (rule 2): no
//! policy can hide a failing dimension (spec §20.3). When the policy names no
//! kind that feeds the dimension, every kind that has evidence counts, again
//! without an age limit.
//!
//! Freshness is judged by the caller from the local head history, exactly as
//! `verify` judges it (the same [`FreshnessJudgement`]); without an anchor it
//! is `UNKNOWN`, whatever was passed. Durability and key availability are
//! `UNKNOWN` (bytes show neither; `health` does not read the descriptor);
//! searchability and retention compliance are `UNSUPPORTED`, as in `verify`.
//! None of these is ever `PASS`.
//!
//! Evidence log lines that cannot be read are reported (a skipped item named
//! `unreadable_evidence`) and **no dimension can be `PASS` while any exist**:
//! a line that cannot be read might have held a `FAIL`.
//!
//! # What the report is
//!
//! The report is schema v1 like every other. Its `level` is `structural` (the
//! level of looking at the head's location, which is all `health` does to the
//! archive) and its `scope` says that no check was run. Wording never says
//! "safe", "backed up", or "preserved" (spec §23.3).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, MochiError, Result};
use crate::report::{
    Finding, FreshnessAnchorKind, FreshnessBasis, Policy, Report, Severity, SkippedItem,
};
use crate::status::{Dimension, Status, VerificationLevel};
use crate::timestamp::Timestamp;
use crate::verify::FreshnessJudgement;

/// Evidence record schema.
pub const EVIDENCE_SCHEMA: u32 = 1;
/// Policy file schema tag.
pub const POLICY_SCHEMA: &str = "mochi-health-policy-v1";
/// The default policy's age limit for `verify` evidence.
pub const DEFAULT_VERIFY_MAX_AGE_DAYS: u32 = 30;

const NANOS_PER_DAY: i128 = 86_400 * 1_000_000_000;

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, msg)
}

/// The command that produced a record. Serialized as typed on the command
/// line: `verify`, `fsck`, `restore-test`, `repair-apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceCommand {
    Verify,
    Fsck,
    RestoreTest,
    /// The re-verification of a new archive written by `repair apply`.
    RepairApply,
}

/// What a policy age limit names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// `verify`, `fsck`, and the re-verification of `repair apply`.
    Verify,
    /// `restore-test`.
    RestoreTest,
}

impl EvidenceCommand {
    pub const fn kind(self) -> EvidenceKind {
        match self {
            EvidenceCommand::Verify | EvidenceCommand::Fsck | EvidenceCommand::RepairApply => {
                EvidenceKind::Verify
            }
            EvidenceCommand::RestoreTest => EvidenceKind::RestoreTest,
        }
    }

    /// Whether this command's records can speak for `d`. Freshness is judged
    /// live, never from a record.
    pub const fn assesses(self, d: Dimension) -> bool {
        match (self, d) {
            (_, Dimension::Recoverability) => true,
            (EvidenceCommand::RestoreTest, _) => false,
            (_, Dimension::Integrity) => true,
            _ => false,
        }
    }
}

impl EvidenceKind {
    const fn feeds(self, d: Dimension) -> bool {
        matches!(
            (self, d),
            (
                EvidenceKind::Verify,
                Dimension::Integrity | Dimension::Recoverability
            ) | (EvidenceKind::RestoreTest, Dimension::Recoverability)
        )
    }

    fn name(self) -> &'static str {
        match self {
            EvidenceKind::Verify => "verify",
            EvidenceKind::RestoreTest => "restore_test",
        }
    }
}

/// The head a run was about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceHead {
    pub seq: u64,
    /// Lower-case hexadecimal commit ID.
    pub commit_id: String,
}

/// One completed run, as it is appended to the evidence log (one JSON object
/// per line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRecord {
    pub schema: u32,
    pub command: EvidenceCommand,
    pub level: Option<VerificationLevel>,
    /// D15 timestamp.
    pub completed_at: String,
    pub head: EvidenceHead,
    /// Every dimension the run reported; `UNKNOWN` means not assessed.
    pub dimensions: BTreeMap<Dimension, Status>,
    pub exit_code: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The stable codes of the run's error findings (so a recorded `FAIL`
    /// can be explained without keeping the whole report).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finding_codes: Vec<ErrorCode>,
}

impl EvidenceRecord {
    /// A record from a finished report. `None` for a run that was
    /// compromised (nothing is concluded from it) or has no completion time.
    pub fn from_report(
        command: EvidenceCommand,
        report: &Report,
        head: EvidenceHead,
    ) -> Option<EvidenceRecord> {
        if report.operational_error {
            return None;
        }
        let mut codes: Vec<ErrorCode> = Vec::new();
        for f in report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
        {
            if !codes.contains(&f.code) {
                codes.push(f.code);
            }
        }
        Some(EvidenceRecord {
            schema: EVIDENCE_SCHEMA,
            command,
            level: Some(report.level),
            completed_at: report.completed_at.clone()?,
            head,
            dimensions: report.dimensions.clone(),
            exit_code: report.exit_code,
            scope: report.scope.clone(),
            finding_codes: codes,
        })
    }

    /// The record is well formed: known schema, a D15 timestamp, a 64-digit
    /// lower-case hexadecimal commit ID. Returns the parsed time.
    pub fn validate(&self) -> Result<Timestamp> {
        if self.schema != EVIDENCE_SCHEMA {
            return Err(invalid(format!(
                "evidence schema {} is not {EVIDENCE_SCHEMA}",
                self.schema
            )));
        }
        let c = &self.head.commit_id;
        if c.len() != 64 || !c.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(invalid("evidence head is not a 64-digit hexadecimal ID"));
        }
        Timestamp::parse(&self.completed_at)
    }
}

/// What the policy requires and how old each kind of evidence may be.
///
/// The file is JSON (K5; the spec's `.yaml` is a non-normative example):
///
/// ```json
/// {"schema": "mochi-health-policy-v1",
///  "required": ["integrity", "recoverability"],
///  "max_age_days": {"verify": 30, "restore_test": 90}}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthPolicy {
    pub schema: String,
    /// Dimensions the exit code depends on (D15). Freshness is also required
    /// when the local history holds this archive.
    pub required: BTreeSet<Dimension>,
    /// A kind named here is **required** to have evidence this recent for
    /// the dimensions it feeds. A kind not named has no age limit and is not
    /// required.
    #[serde(default)]
    pub max_age_days: BTreeMap<EvidenceKind, u32>,
}

impl Default for HealthPolicy {
    /// Integrity and recoverability required; `verify` evidence (any level
    /// that reads stored bytes) at most 30 days old.
    fn default() -> Self {
        HealthPolicy {
            schema: POLICY_SCHEMA.to_owned(),
            required: [Dimension::Integrity, Dimension::Recoverability].into(),
            max_age_days: [(EvidenceKind::Verify, DEFAULT_VERIFY_MAX_AGE_DAYS)].into(),
        }
    }
}

impl HealthPolicy {
    /// A parsed policy is usable: the schema tag matches and something is
    /// required (a policy that requires nothing could never fail).
    pub fn validate(&self) -> Result<()> {
        if self.schema != POLICY_SCHEMA {
            return Err(invalid(format!(
                "health policy schema {:?} is not {POLICY_SCHEMA:?}",
                self.schema
            )));
        }
        if self.required.is_empty() {
            return Err(invalid(
                "a health policy must require at least one dimension",
            ));
        }
        Ok(())
    }
}

/// The archive's current head, as the caller located it (no verification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentHead {
    pub archive_id: String,
    pub seq: u64,
    pub commit_id: String,
}

/// The local-history anchor, when this client has seen the archive before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRef {
    pub seq: u64,
    pub commit_id: String,
}

/// Everything [`assess`] needs; it does no I/O.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    /// Every readable record of this archive, in any order.
    pub evidence: &'a [EvidenceRecord],
    /// Log lines that could not be read.
    pub unreadable_evidence: u64,
    /// Why no evidence could be consulted (for example local history is
    /// off); the dimensions are then `UNKNOWN` and the reason is reported.
    pub evidence_unavailable: Option<String>,
    pub current: &'a CurrentHead,
    pub anchor: Option<AnchorRef>,
    /// The caller's D8 verdict on `anchor` against the archive's history.
    pub freshness: FreshnessJudgement,
    /// Why freshness could not be judged, when it could not.
    pub freshness_note: Option<String>,
    pub now: Timestamp,
}

fn nanos(t: &Timestamp) -> i128 {
    i128::from(t.unix_secs()) * 1_000_000_000 + i128::from(t.subsec_nanos())
}

fn dim_name(d: Dimension) -> &'static str {
    match d {
        Dimension::Durability => "durability",
        Dimension::Integrity => "integrity",
        Dimension::Recoverability => "recoverability",
        Dimension::Searchability => "searchability",
        Dimension::Freshness => "freshness",
        Dimension::RetentionCompliance => "retention_compliance",
        Dimension::KeyAvailability => "key_availability",
    }
}

fn cmd_name(c: EvidenceCommand) -> &'static str {
    match c {
        EvidenceCommand::Verify => "verify",
        EvidenceCommand::Fsck => "fsck",
        EvidenceCommand::RestoreTest => "restore-test",
        EvidenceCommand::RepairApply => "repair apply",
    }
}

fn note(d: Dimension, reason: impl Into<String>) -> SkippedItem {
    SkippedItem {
        item: dim_name(d).to_owned(),
        reason: reason.into(),
    }
}

/// One dimension's result from one kind of evidence (or from any kind).
struct Outcome {
    status: Status,
    findings: Vec<Finding>,
    notes: Vec<SkippedItem>,
    /// Age in seconds of the record that decided it.
    age: Option<u64>,
    /// Some record of this kind assessed the dimension.
    has_evidence: bool,
}

fn days(n: i128) -> String {
    format!("{} day(s)", n / NANOS_PER_DAY)
}

/// The rules above for dimension `d`, over the records of `kind` (any kind
/// for `None`), with the age limit `limit` (none for no limit).
fn judge(
    d: Dimension,
    records: &[(&EvidenceRecord, Timestamp)],
    kind: EvidenceKind,
    limit: Option<u32>,
    inp: &Inputs<'_>,
) -> Outcome {
    let what = kind.name();
    let latest = records
        .iter()
        .filter(|(r, _)| r.command.assesses(d))
        .filter(|(r, _)| r.command.kind() == kind)
        .filter(|(r, _)| r.dimensions.get(&d).is_some_and(|s| *s != Status::Unknown))
        .max_by_key(|(_, t)| *t);
    let Some((rec, at)) = latest else {
        return Outcome {
            status: Status::Unknown,
            findings: Vec::new(),
            notes: vec![note(
                d,
                format!("no {what} evidence for this dimension is recorded for this archive"),
            )],
            age: None,
            has_evidence: false,
        };
    };
    let recorded = rec.dimensions[&d];
    let age_ns = nanos(&inp.now) - nanos(at);
    let age = u64::try_from(age_ns.max(0) / 1_000_000_000).ok();
    let by = format!(
        "{} on {} at commit {}",
        cmd_name(rec.command),
        rec.completed_at,
        rec.head.seq
    );
    let same_head = rec.head.seq == inp.current.seq && rec.head.commit_id == inp.current.commit_id;

    if recorded == Status::Fail {
        let message = if same_head {
            format!("{by} found a failure that no later {what} evidence has replaced")
        } else {
            format!(
                "{by} found a failure that no later {what} evidence has replaced; the current \
                 head is commit {}",
                inp.current.seq
            )
        };
        let mut findings: Vec<Finding> = rec
            .finding_codes
            .iter()
            .map(|code| Finding {
                code: *code,
                severity: Severity::Error,
                message: Some(message.clone()),
                expected: None,
                observed: None,
                affected: None,
            })
            .collect();
        let mut notes = Vec::new();
        if findings.is_empty() {
            notes.push(note(d, message));
        }
        findings.dedup_by(|a, b| a.code == b.code);
        return Outcome {
            status: Status::Fail,
            findings,
            notes,
            age,
            has_evidence: true,
        };
    }
    if !same_head {
        return Outcome {
            status: Status::Unknown,
            findings: Vec::new(),
            notes: vec![note(
                d,
                format!(
                    "the latest {what} evidence ({by}) is about commit {} ({}), and the current \
                     head is commit {} ({}); it says nothing about the commits added since",
                    rec.head.seq, rec.head.commit_id, inp.current.seq, inp.current.commit_id
                ),
            )],
            age,
            has_evidence: true,
        };
    }
    if age_ns < 0 {
        return Outcome {
            status: Status::Unknown,
            findings: Vec::new(),
            notes: vec![note(
                d,
                format!(
                    "the latest {what} evidence ({by}) is dated after the current time; the \
                     clock or the evidence log cannot be trusted"
                ),
            )],
            age,
            has_evidence: true,
        };
    }
    if let Some(limit) = limit {
        if age_ns > i128::from(limit) * NANOS_PER_DAY {
            return Outcome {
                status: Status::Overdue,
                findings: Vec::new(),
                notes: vec![note(
                    d,
                    format!(
                        "the latest {what} evidence ({by}) is {} old; the policy allows {limit} \
                         day(s)",
                        days(age_ns)
                    ),
                )],
                age,
                has_evidence: true,
            };
        }
    }
    Outcome {
        status: recorded,
        findings: Vec::new(),
        notes: Vec::new(),
        age,
        has_evidence: true,
    }
}

/// Assess one archive's health from recorded evidence.
///
/// The report is schema v1 and always passes [`Report::validate`]; its three
/// D15 results come from [`Report::conclude`].
pub fn assess(inp: &Inputs<'_>, policy: &HealthPolicy) -> Report {
    let mut r = Report::new(VerificationLevel::Structural);
    let now = inp.now.to_string();
    r.started_at = Some(now.clone());
    r.completed_at = Some(now);
    r.archive_id = Some(inp.current.archive_id.clone());
    r.checked_commit = Some(inp.current.commit_id.clone());
    r.scope = Some(
        "a summary of evidence recorded on this machine by earlier runs, judged for the current \
         head: no check of the archive was run and no stored byte was read"
            .to_owned(),
    );
    if let Some(a) = &inp.anchor {
        r.expected_head = Some(a.commit_id.clone());
        r.freshness_anchor = FreshnessAnchorKind::LocalHistory;
    }
    r.policy = Policy::new(
        policy.required.iter().copied(),
        FreshnessBasis {
            archive_in_local_history: inp.anchor.is_some(),
            ..FreshnessBasis::default()
        },
    );

    // Records that cannot be used are counted with the unreadable log lines.
    let mut unreadable = inp.unreadable_evidence;
    let mut parsed: Vec<(&EvidenceRecord, Timestamp)> = Vec::new();
    if inp.evidence_unavailable.is_none() {
        for rec in inp.evidence {
            match rec.validate() {
                Ok(t) => parsed.push((rec, t)),
                Err(_) => unreadable += 1,
            }
        }
    }

    let mut oldest: Option<u64> = None;
    for d in [Dimension::Integrity, Dimension::Recoverability] {
        let mut out = if let Some(why) = &inp.evidence_unavailable {
            Outcome {
                status: Status::Unknown,
                findings: Vec::new(),
                notes: vec![note(d, why.clone())],
                age: None,
                has_evidence: false,
            }
        } else {
            let feeding: Vec<EvidenceKind> = [EvidenceKind::Verify, EvidenceKind::RestoreTest]
                .into_iter()
                .filter(|k| k.feeds(d))
                .collect();
            let any_required = feeding.iter().any(|k| policy.max_age_days.contains_key(k));
            let mut outs: Vec<Outcome> = Vec::new();
            for k in feeding {
                let limit = policy.max_age_days.get(&k).copied();
                let o = judge(d, &parsed, k, limit, inp);
                // A named kind is required; an unnamed one counts when
                // nothing is named, and always when it recorded a failure.
                let counts = limit.is_some()
                    || (o.has_evidence && (!any_required || o.status == Status::Fail));
                if counts {
                    outs.push(o);
                }
            }
            if outs.is_empty() {
                outs.push(Outcome {
                    status: Status::Unknown,
                    findings: Vec::new(),
                    notes: vec![note(
                        d,
                        "no evidence for this dimension is recorded for this archive",
                    )],
                    age: None,
                    has_evidence: false,
                });
            }
            Outcome {
                status: Status::rollup(outs.iter().map(|o| o.status)),
                age: outs.iter().filter_map(|o| o.age).max(),
                findings: outs.iter().flat_map(|o| o.findings.clone()).collect(),
                notes: outs.into_iter().flat_map(|o| o.notes).collect(),
                has_evidence: true,
            }
        };
        if unreadable > 0 && out.status == Status::Pass {
            out.status = Status::Unknown;
            out.notes.push(note(
                d,
                "evidence log lines could not be read, and one might have recorded a failure",
            ));
        }
        oldest = oldest.max(out.age);
        r.dimensions.insert(d, out.status);
        r.findings.extend(out.findings);
        r.skipped.extend(out.notes);
    }
    r.coverage.evidence_age_seconds = oldest;
    if unreadable > 0 {
        r.skipped.push(SkippedItem {
            item: "unreadable_evidence".to_owned(),
            reason: format!("{unreadable} evidence log line(s) could not be read and were ignored"),
        });
    }

    // Freshness: the caller's D8 verdict, but never without an anchor.
    let (fresh, findings) = if inp.anchor.is_some() {
        (inp.freshness.status, inp.freshness.findings.clone())
    } else {
        (Status::Unknown, Vec::new())
    };
    r.dimensions.insert(Dimension::Freshness, fresh);
    r.findings.extend(findings);
    if let Some(why) = &inp.freshness_note {
        r.skipped.push(note(Dimension::Freshness, why.clone()));
    } else if inp.anchor.is_none() {
        r.skipped.push(note(
            Dimension::Freshness,
            "this client has not seen this archive before, so there is no anchor to compare the \
             head with",
        ));
    }

    // Dimensions no 1.0 evidence covers, as `verify` reports them.
    r.dimensions.insert(Dimension::Durability, Status::Unknown);
    r.dimensions
        .insert(Dimension::Searchability, Status::Unsupported);
    r.dimensions
        .insert(Dimension::RetentionCompliance, Status::Unsupported);
    r.dimensions
        .insert(Dimension::KeyAvailability, Status::Unknown);
    r.conclude(false);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "11111111111111111111111111111111111111111111111111111111111111aa";
    const OLD: &str = "22222222222222222222222222222222222222222222222222222222222222bb";

    fn ts(secs: i64, nanos: u32) -> Timestamp {
        Timestamp::from_unix(secs, nanos).unwrap()
    }

    const DAY: i64 = 86_400;
    const T0: i64 = 1_700_000_000;

    fn current() -> CurrentHead {
        CurrentHead {
            archive_id: "aa".repeat(32),
            seq: 5,
            commit_id: HEAD.into(),
        }
    }

    fn rec(
        command: EvidenceCommand,
        at: Timestamp,
        seq: u64,
        dims: &[(Dimension, Status)],
    ) -> EvidenceRecord {
        EvidenceRecord {
            schema: EVIDENCE_SCHEMA,
            command,
            level: Some(VerificationLevel::Restoration),
            completed_at: at.to_string(),
            head: EvidenceHead {
                seq,
                commit_id: if seq == 5 { HEAD } else { OLD }.into(),
            },
            dimensions: dims.iter().copied().collect(),
            exit_code: 0,
            scope: None,
            finding_codes: Vec::new(),
        }
    }

    fn pass_both(command: EvidenceCommand, at: Timestamp, seq: u64) -> EvidenceRecord {
        rec(
            command,
            at,
            seq,
            &[
                (Dimension::Integrity, Status::Pass),
                (Dimension::Recoverability, Status::Pass),
            ],
        )
    }

    fn run(evidence: &[EvidenceRecord], policy: &HealthPolicy, now: Timestamp) -> Report {
        run_with(evidence, policy, now, 0, None)
    }

    fn run_with(
        evidence: &[EvidenceRecord],
        policy: &HealthPolicy,
        now: Timestamp,
        unreadable: u64,
        anchor: Option<(AnchorRef, Status)>,
    ) -> Report {
        let cur = current();
        let (a, status) = match anchor {
            Some((a, s)) => (Some(a), s),
            None => (None, Status::Pass), // a Pass without an anchor must be ignored
        };
        let r = assess(
            &Inputs {
                evidence,
                unreadable_evidence: unreadable,
                evidence_unavailable: None,
                current: &cur,
                anchor: a,
                freshness: FreshnessJudgement {
                    status,
                    findings: Vec::new(),
                },
                freshness_note: None,
                now,
            },
            policy,
        );
        r.validate()
            .unwrap_or_else(|v| panic!("report breaks the invariants: {v:?}"));
        r
    }

    fn dim(r: &Report, d: Dimension) -> Status {
        r.dimensions[&d]
    }

    fn now() -> Timestamp {
        ts(T0 + 10 * DAY, 0)
    }

    #[test]
    fn no_evidence_is_unknown_and_exits_2() {
        let r = run(&[], &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown);
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Unknown);
        assert_eq!(r.exit_code, 2);
        assert!(r.skipped.iter().any(|s| s.item == "integrity"));
        assert_eq!(r.coverage.evidence_age_seconds, None);
    }

    #[test]
    fn current_evidence_passes_but_the_report_is_never_an_overall_pass() {
        let ev = [pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 5)];
        let r = run(&ev, &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Pass);
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Pass);
        assert_eq!(r.policy_result, Status::Pass);
        assert_eq!(r.exit_code, 0, "{:?}", r.skipped);
        // Durability, key availability, searchability, retention are not PASS.
        assert_ne!(r.overall_status, Status::Pass);
        for d in [
            Dimension::Durability,
            Dimension::Searchability,
            Dimension::RetentionCompliance,
            Dimension::KeyAvailability,
        ] {
            assert_ne!(dim(&r, d), Status::Pass, "{d:?}");
        }
        assert_eq!(dim(&r, Dimension::Searchability), Status::Unsupported);
        assert_eq!(r.coverage.evidence_age_seconds, Some(DAY as u64));
    }

    #[test]
    fn commits_added_since_make_a_pass_unknown_and_name_both_heads() {
        let ev = [pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 4)];
        let r = run(&ev, &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown);
        assert_eq!(r.exit_code, 2);
        let why = &r
            .skipped
            .iter()
            .find(|s| s.item == "integrity")
            .unwrap()
            .reason;
        assert!(why.contains(OLD) && why.contains(HEAD), "{why}");
    }

    #[test]
    fn a_recorded_failure_stands_across_new_commits_and_age() {
        let mut bad = rec(
            EvidenceCommand::Fsck,
            ts(T0 - 400 * DAY, 0),
            3,
            &[
                (Dimension::Integrity, Status::Fail),
                (Dimension::Recoverability, Status::Fail),
            ],
        );
        bad.finding_codes = vec![ErrorCode::StoredIntegrityFailed];
        let r = run(&[bad], &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Fail);
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Fail);
        assert_eq!(r.exit_code, 1);
        assert!(r
            .findings
            .iter()
            .any(|f| f.code == ErrorCode::StoredIntegrityFailed && f.severity == Severity::Error));
        let m = r.findings[0].message.as_deref().unwrap();
        assert!(
            m.contains("fsck") && m.contains("current head is commit 5"),
            "{m}"
        );
    }

    #[test]
    fn the_latest_evidence_wins_in_both_directions() {
        let fail = |at| {
            let mut f = rec(
                EvidenceCommand::Verify,
                at,
                5,
                &[
                    (Dimension::Integrity, Status::Fail),
                    (Dimension::Recoverability, Status::Pass),
                ],
            );
            f.finding_codes = vec![ErrorCode::ContentIntegrityFailed];
            f
        };
        // FAIL older than a newer PASS: the PASS replaces it.
        let ev = [
            fail(ts(T0 + 5 * DAY, 0)),
            pass_both(EvidenceCommand::Verify, ts(T0 + 8 * DAY, 0), 5),
        ];
        let r = run(&ev, &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Pass);
        // PASS older than a newer FAIL: the FAIL stands.
        let ev = [
            pass_both(EvidenceCommand::Verify, ts(T0 + 5 * DAY, 0), 5),
            fail(ts(T0 + 8 * DAY, 0)),
        ];
        let r = run(&ev, &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Fail);
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn expired_evidence_is_overdue_and_the_limit_is_strict() {
        let done = ts(T0, 0);
        let ev = [pass_both(EvidenceCommand::Verify, done, 5)];
        let policy = HealthPolicy::default();
        // Exactly at the limit: still current.
        let at_limit = ts(T0 + 30 * DAY, 0);
        let r = run(&ev, &policy, at_limit);
        assert_eq!(dim(&r, Dimension::Integrity), Status::Pass);
        // One nanosecond over: overdue.
        let over = ts(T0 + 30 * DAY, 1);
        let r = run(&ev, &policy, over);
        assert_eq!(dim(&r, Dimension::Integrity), Status::Overdue);
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Overdue);
        assert_eq!(r.policy_result, Status::Overdue);
        assert_eq!(r.exit_code, 2);
        // A limit of zero days: any age at all is overdue.
        let zero = HealthPolicy {
            max_age_days: [(EvidenceKind::Verify, 0)].into(),
            ..HealthPolicy::default()
        };
        assert_eq!(
            dim(&run(&ev, &zero, ts(T0, 1)), Dimension::Integrity),
            Status::Overdue
        );
        assert_eq!(
            dim(&run(&ev, &zero, ts(T0, 0)), Dimension::Integrity),
            Status::Pass
        );
    }

    #[test]
    fn evidence_from_the_future_is_unknown() {
        let ev = [pass_both(EvidenceCommand::Verify, ts(T0 + 20 * DAY, 0), 5)];
        let r = run(&ev, &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown);
        assert_eq!(r.exit_code, 2);
    }

    #[test]
    fn a_run_that_did_not_assess_a_dimension_does_not_erase_an_earlier_pass() {
        let earlier = pass_both(EvidenceCommand::Verify, ts(T0 + 5 * DAY, 0), 5);
        // A later structural verify: integrity and recoverability UNKNOWN.
        let later = rec(
            EvidenceCommand::Verify,
            ts(T0 + 9 * DAY, 0),
            5,
            &[
                (Dimension::Integrity, Status::Unknown),
                (Dimension::Recoverability, Status::Unknown),
            ],
        );
        let r = run(&[earlier, later], &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Pass);
        // ...and the PASS keeps its own age (5 days), not the later run's.
        assert_eq!(r.coverage.evidence_age_seconds, Some(5 * DAY as u64));
    }

    #[test]
    fn naming_restore_test_requires_restore_evidence() {
        let policy = HealthPolicy {
            max_age_days: [(EvidenceKind::Verify, 30), (EvidenceKind::RestoreTest, 90)].into(),
            ..HealthPolicy::default()
        };
        let verify = pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 5);
        // Fresh verify alone: recoverability needs restore evidence too.
        let r = run(std::slice::from_ref(&verify), &policy, now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Pass);
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Unknown);
        // A fresh restore-test completes it...
        let restore = rec(
            EvidenceCommand::RestoreTest,
            ts(T0 + 9 * DAY, 0),
            5,
            &[(Dimension::Recoverability, Status::Pass)],
        );
        let r = run(&[verify.clone(), restore.clone()], &policy, now());
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Pass);
        // ...and a restore-test says nothing about integrity, even if a record
        // were to carry an integrity status.
        let r = run(std::slice::from_ref(&restore), &policy, now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown);
        let odd = rec(
            EvidenceCommand::RestoreTest,
            ts(T0 + 9 * DAY, 0),
            5,
            &[
                (Dimension::Integrity, Status::Pass),
                (Dimension::Recoverability, Status::Pass),
            ],
        );
        let r = run(&[odd], &policy, now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown);
        // An expired restore-test makes recoverability overdue though verify is fresh.
        let old = rec(
            EvidenceCommand::RestoreTest,
            ts(T0 - 200 * DAY, 0),
            5,
            &[(Dimension::Recoverability, Status::Pass)],
        );
        let r = run(&[verify, old], &policy, now());
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Overdue);
        // Without the key, restore-test evidence is neither required nor needed.
        let r = run(
            &[pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 5)],
            &HealthPolicy::default(),
            now(),
        );
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Pass);
    }

    /// A kind the policy does not name is not required and has no age limit,
    /// but its recorded failure still stands: no policy hides a failure.
    #[test]
    fn a_failure_of_a_kind_the_policy_does_not_name_still_stands() {
        let mut restore_fail = rec(
            EvidenceCommand::RestoreTest,
            ts(T0 - 300 * DAY, 0),
            2,
            &[(Dimension::Recoverability, Status::Fail)],
        );
        restore_fail.finding_codes = vec![ErrorCode::StoredIntegrityFailed];
        let verify = pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 5);

        // Default policy (names only `verify`): recoverability FAILs on the
        // old restore failure; integrity, which a restore-test never speaks
        // for, is untouched.
        let r = run(
            &[verify.clone(), restore_fail.clone()],
            &HealthPolicy::default(),
            now(),
        );
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Fail);
        assert_eq!(dim(&r, Dimension::Integrity), Status::Pass);
        assert_eq!(r.exit_code, 1);
        assert!(r
            .findings
            .iter()
            .any(|f| f.code == ErrorCode::StoredIntegrityFailed));

        // A restore-test that passed is neither required nor counted there:
        // without `verify` evidence, recoverability is UNKNOWN.
        let restore_ok = rec(
            EvidenceCommand::RestoreTest,
            ts(T0 + 9 * DAY, 0),
            5,
            &[(Dimension::Recoverability, Status::Pass)],
        );
        let r = run(
            std::slice::from_ref(&restore_ok),
            &HealthPolicy::default(),
            now(),
        );
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Unknown);

        // A policy naming only `restore_test`: integrity comes from `verify`
        // with no age limit (any status counts); recoverability is the
        // restore-test's, except that a recorded `verify` failure stands.
        let only_restore = HealthPolicy {
            max_age_days: [(EvidenceKind::RestoreTest, 90)].into(),
            ..HealthPolicy::default()
        };
        let old_verify = pass_both(EvidenceCommand::Verify, ts(T0 - 500 * DAY, 0), 5);
        let r = run(
            &[old_verify.clone(), restore_ok.clone()],
            &only_restore,
            now(),
        );
        assert_eq!(
            dim(&r, Dimension::Integrity),
            Status::Pass,
            "no age limit on verify"
        );
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Pass);
        let mut verify_fail = rec(
            EvidenceCommand::Verify,
            ts(T0 + 9 * DAY, 0),
            5,
            &[
                (Dimension::Integrity, Status::Pass),
                (Dimension::Recoverability, Status::Fail),
            ],
        );
        verify_fail.finding_codes = vec![ErrorCode::ContentIntegrityFailed];
        let r = run(&[verify_fail, restore_ok], &only_restore, now());
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Fail);
    }

    #[test]
    fn repair_apply_evidence_counts_for_both_dimensions() {
        let ev = [pass_both(
            EvidenceCommand::RepairApply,
            ts(T0 + 9 * DAY, 0),
            5,
        )];
        let r = run(&ev, &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Pass);
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Pass);
    }

    #[test]
    fn unreadable_evidence_is_reported_and_blocks_a_pass() {
        let ev = [pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 5)];
        let r = run_with(&ev, &HealthPolicy::default(), now(), 2, None);
        assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown);
        assert_eq!(dim(&r, Dimension::Recoverability), Status::Unknown);
        let s = r
            .skipped
            .iter()
            .find(|s| s.item == "unreadable_evidence")
            .unwrap();
        assert!(s.reason.contains('2'), "{}", s.reason);
        assert_eq!(r.exit_code, 2);
        // A recorded failure still shows through.
        let mut bad = rec(
            EvidenceCommand::Verify,
            ts(T0 + 9 * DAY, 0),
            5,
            &[(Dimension::Integrity, Status::Fail)],
        );
        bad.finding_codes = vec![ErrorCode::StoredIntegrityFailed];
        let r = run_with(&[bad], &HealthPolicy::default(), now(), 1, None);
        assert_eq!(dim(&r, Dimension::Integrity), Status::Fail);
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn a_malformed_record_is_counted_not_trusted() {
        let mut bad = pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 5);
        bad.completed_at = "yesterday".into();
        let r = run(&[bad], &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown);
        assert!(r.skipped.iter().any(|s| s.item == "unreadable_evidence"));
    }

    #[test]
    fn freshness_follows_the_anchor_and_never_passes_without_one() {
        let ev = [pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 5)];
        // No anchor: UNKNOWN even if the caller passed PASS; not required.
        let r = run(&ev, &HealthPolicy::default(), now());
        assert_eq!(dim(&r, Dimension::Freshness), Status::Unknown);
        assert_eq!(r.freshness_anchor, FreshnessAnchorKind::None);
        assert!(!r.policy.required.contains(&Dimension::Freshness));
        assert_eq!(r.exit_code, 0);
        // An anchor and a PASS verdict: required, named, and PASS.
        let a = AnchorRef {
            seq: 4,
            commit_id: OLD.into(),
        };
        let r = run_with(
            &ev,
            &HealthPolicy::default(),
            now(),
            0,
            Some((a.clone(), Status::Pass)),
        );
        assert_eq!(dim(&r, Dimension::Freshness), Status::Pass);
        assert_eq!(r.freshness_anchor, FreshnessAnchorKind::LocalHistory);
        assert_eq!(r.expected_head.as_deref(), Some(OLD));
        assert!(r.policy.required.contains(&Dimension::Freshness));
        assert_eq!(r.exit_code, 0);
        // A failed verdict fails the report.
        let r = run_with(
            &ev,
            &HealthPolicy::default(),
            now(),
            0,
            Some((a, Status::Fail)),
        );
        assert_eq!(dim(&r, Dimension::Freshness), Status::Fail);
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn requiring_a_dimension_nothing_covers_is_unsupported_exit_4() {
        let ev = [pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 5)];
        let policy = HealthPolicy {
            required: [Dimension::Integrity, Dimension::Searchability].into(),
            ..HealthPolicy::default()
        };
        let r = run(&ev, &policy, now());
        assert_eq!(r.policy_result, Status::Unsupported);
        assert_eq!(r.exit_code, 4);
    }

    #[test]
    fn disabled_local_history_is_reported_not_guessed() {
        let cur = current();
        let r = assess(
            &Inputs {
                evidence: &[],
                unreadable_evidence: 0,
                evidence_unavailable: Some("local history is off (--no-local-history)".into()),
                current: &cur,
                anchor: None,
                freshness: FreshnessJudgement {
                    status: Status::Unknown,
                    findings: Vec::new(),
                },
                freshness_note: None,
                now: now(),
            },
            &HealthPolicy::default(),
        );
        r.validate().unwrap();
        assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown);
        assert!(r
            .skipped
            .iter()
            .any(|s| s.reason.contains("--no-local-history")));
    }

    #[test]
    fn nothing_says_safe_backed_up_or_preserved() {
        let bad = |dims: &[(Dimension, Status)]| {
            let mut e = rec(EvidenceCommand::Fsck, ts(T0 + 9 * DAY, 0), 4, dims);
            e.finding_codes = vec![ErrorCode::StoredIntegrityFailed];
            e
        };
        let cases: Vec<Vec<EvidenceRecord>> = vec![
            vec![],
            vec![pass_both(EvidenceCommand::Verify, ts(T0, 0), 5)],
            vec![pass_both(EvidenceCommand::Verify, ts(T0 + 9 * DAY, 0), 4)],
            vec![bad(&[(Dimension::Integrity, Status::Fail)])],
        ];
        for ev in &cases {
            let r = run_with(ev, &HealthPolicy::default(), now(), 1, None);
            let mut text = String::new();
            for s in &r.skipped {
                text.push_str(&s.reason);
            }
            for f in &r.findings {
                text.push_str(f.message.as_deref().unwrap_or(""));
            }
            text.push_str(r.scope.as_deref().unwrap_or(""));
            let lower = text.to_lowercase();
            for word in ["safe", "backed up", "preserved"] {
                assert!(!lower.contains(word), "{word:?} in {text:?}");
            }
        }
    }

    // ---- files -----------------------------------------------------------

    #[test]
    fn policy_files_are_strict() {
        let ok = r#"{"schema":"mochi-health-policy-v1","required":["integrity","freshness"],
                     "max_age_days":{"verify":7,"restore_test":90}}"#;
        let p: HealthPolicy = serde_json::from_str(ok).unwrap();
        p.validate().unwrap();
        assert_eq!(p.max_age_days[&EvidenceKind::RestoreTest], 90);
        assert!(p.required.contains(&Dimension::Freshness));
        // Without max_age_days: no age limits.
        let p: HealthPolicy =
            serde_json::from_str(r#"{"schema":"mochi-health-policy-v1","required":["integrity"]}"#)
                .unwrap();
        assert!(p.max_age_days.is_empty());

        for bad in [
            // unknown keys, at every level
            r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"extra":1}"#,
            r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"max_age_days":{"scrub":1}}"#,
            // unknown dimension
            r#"{"schema":"mochi-health-policy-v1","required":["integrity","backup"]}"#,
            // not a number of days
            r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"max_age_days":{"verify":-1}}"#,
            r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"max_age_days":{"verify":"30"}}"#,
            // missing required list
            r#"{"schema":"mochi-health-policy-v1"}"#,
        ] {
            assert!(serde_json::from_str::<HealthPolicy>(bad).is_err(), "{bad}");
        }
        // Parsed but unusable.
        for bad in [
            r#"{"schema":"other","required":["integrity"]}"#,
            r#"{"schema":"mochi-health-policy-v1","required":[]}"#,
        ] {
            let p: HealthPolicy = serde_json::from_str(bad).unwrap();
            assert_eq!(
                p.validate().unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{bad}"
            );
        }
        HealthPolicy::default().validate().unwrap();
    }

    #[test]
    fn evidence_records_round_trip_and_reject_what_is_malformed() {
        let mut e = pass_both(EvidenceCommand::RepairApply, ts(T0, 5), 5);
        e.finding_codes = vec![ErrorCode::ReferenceInvalid];
        e.scope = Some("x".into());
        let line = serde_json::to_string(&e).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains("\"command\":\"repair-apply\""), "{line}");
        let back: EvidenceRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back, e);
        back.validate().unwrap();

        let mut bad = e.clone();
        bad.schema = 2;
        assert!(bad.validate().is_err());
        let mut bad = e.clone();
        bad.head.commit_id = "ABC".into();
        assert!(bad.validate().is_err());
        let mut bad = e.clone();
        bad.head.commit_id = HEAD.to_uppercase();
        assert!(bad.validate().is_err());
        let mut bad = e;
        bad.completed_at = "2026-10-08".into();
        assert!(bad.validate().is_err());
        // Unknown keys are refused.
        assert!(
            serde_json::from_str::<EvidenceRecord>(&line.replacen("{", "{\"surprise\":1,", 1))
                .is_err()
        );
    }

    #[test]
    fn a_record_from_a_report_keeps_its_dimensions_and_error_codes_only() {
        let mut report = Report::new(VerificationLevel::StoredIntegrity);
        report.completed_at = Some(ts(T0, 0).to_string());
        report.dimensions.insert(Dimension::Integrity, Status::Fail);
        report.findings.push(Finding {
            code: ErrorCode::StoredIntegrityFailed,
            severity: Severity::Error,
            message: Some("a message".into()),
            expected: None,
            observed: None,
            affected: None,
        });
        report.findings.push(Finding {
            code: ErrorCode::FreshnessFailed,
            severity: Severity::Info,
            message: None,
            expected: None,
            observed: None,
            affected: None,
        });
        report.findings.push(report.findings[0].clone());
        report.conclude(false);
        let head = EvidenceHead {
            seq: 5,
            commit_id: HEAD.into(),
        };
        let e =
            EvidenceRecord::from_report(EvidenceCommand::Verify, &report, head.clone()).unwrap();
        assert_eq!(e.finding_codes, [ErrorCode::StoredIntegrityFailed]);
        assert_eq!(e.level, Some(VerificationLevel::StoredIntegrity));
        assert_eq!(e.dimensions[&Dimension::Integrity], Status::Fail);
        assert_eq!(e.exit_code, 1);
        e.validate().unwrap();
        // A compromised run concludes nothing.
        report.conclude(true);
        assert!(EvidenceRecord::from_report(EvidenceCommand::Verify, &report, head).is_none());
    }
}
