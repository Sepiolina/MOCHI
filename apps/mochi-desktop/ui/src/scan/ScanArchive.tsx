import { useEffect, useId, useRef, useState } from "react";

import { Panel } from "../a11y/Panel";
import { errorText } from "../browser/format";
import { formatNumber, t } from "../i18n";
import type { ArchiveSummary } from "../ipc/browse";
import { cancelJob, startJob } from "../ipc/jobs";
import type { JobEvent } from "../ipc/jobs";
import { pickScannerProgram, scanSetup } from "../ipc/scan";
import type { ScanReportView, ScannerView, ScanVerdict } from "../ipc/scan";

type Phase =
  | { at: "form" }
  | { at: "running"; job: number | null; scanning: boolean; cancelling: boolean }
  | { at: "done"; report: ScanReportView }
  | { at: "cancelled" }
  | { at: "failed"; message: string };

/** What the scanner reported, in words. Never MOCHI's own verdict (spec §23.3). */
export function verdictText(r: Pick<ScanReportView, "scanner" | "verdict" | "exit_code">): string {
  const scanner = r.scanner;
  if (r.exit_code === null) return t("scan.verdict.noCode", { scanner });
  const code = String(r.exit_code);
  const v: ScanVerdict = r.verdict;
  switch (v) {
    case "none_reported":
      return t("scan.verdict.none_reported", { scanner });
    case "threats_reported":
      return t("scan.verdict.threats_reported", { scanner });
    case "scanner_failed":
      return t("scan.verdict.scanner_failed", { scanner, code });
    case "unknown":
      return t("scan.verdict.unknown", { scanner, code });
  }
}

/** The index in `detected` that `chosen` is, or -1 for another program. */
function matchIndex(detected: ScannerView[], chosen: ScannerView): number {
  return detected.findIndex((d) => d.program === chosen.program);
}

/**
 * Scan for viruses (owner request 2026-11-14, after WinRAR's window): the scanners found on
 * this computer, the program and its parameters (`%f` is the folder to scan), and then the
 * scanner's own report. MOCHI runs the user's scanner on a temporary extraction of the version
 * on screen and says only what the scanner reported: no success styling, and never "safe".
 * The scanner's output is shown as text.
 */
export function ScanArchive({
  archive,
  seq,
  onClose,
}: {
  archive: ArchiveSummary;
  seq: number;
  onClose: () => void;
}) {
  const id = useId();
  const [detected, setDetected] = useState<ScannerView[]>([]);
  const [choice, setChoice] = useState(-1);
  const [program, setProgram] = useState("");
  const [args, setArgs] = useState("");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [phase, setPhase] = useState<Phase>({ at: "form" });
  const liveRef = useRef(true);
  const jobRef = useRef<number | null>(null);

  useEffect(() => {
    liveRef.current = true;
    scanSetup().then(
      (s) => {
        if (!liveRef.current) return;
        setDetected(s.detected);
        setChoice(matchIndex(s.detected, s.chosen));
        setProgram(s.chosen.program);
        setArgs(s.chosen.arguments);
      },
      (e: unknown) => {
        if (liveRef.current) setLoadError(errorText(t("scan.loadFailed"), e));
      },
    );
    return () => {
      liveRef.current = false;
      // Closing the window stops a scan that is still running.
      if (jobRef.current !== null) void cancelJob(jobRef.current);
    };
  }, []);

  const choose = (value: string) => {
    const i = Number(value);
    setChoice(i);
    const d = detected[i];
    if (d !== undefined) {
      setProgram(d.program);
      setArgs(d.arguments);
    }
  };

  const browse = () => {
    pickScannerProgram().then(
      (p) => {
        if (p !== null && liveRef.current) {
          setProgram(p);
          setChoice(-1);
        }
      },
      (e: unknown) => {
        if (liveRef.current) setPhase({ at: "failed", message: errorText(t("scan.failed"), e) });
      },
    );
  };

  const onEvent = (e: JobEvent) => {
    if (!liveRef.current) return;
    if (e.event === "progress") {
      const scanning = e.phase === "scan";
      setPhase((p) => (p.at === "running" ? { ...p, scanning } : p));
      return;
    }
    if (e.event !== "finished") return;
    jobRef.current = null;
    const o = e.outcome;
    if (o.status === "completed" && o.scan) setPhase({ at: "done", report: o.scan });
    else if (o.status === "cancelled") setPhase({ at: "cancelled" });
    else if (o.status === "failed") setPhase({ at: "failed", message: errorText(t("scan.failed"), o) });
    else setPhase({ at: "failed", message: t("scan.failed") });
  };

  const start = () => {
    setPhase({ at: "running", job: null, scanning: false, cancelling: false });
    startJob({ kind: "scan_archive", handle: archive.handle, seq, program, arguments: args }, onEvent).then(
      (job) => {
        jobRef.current = job;
        if (liveRef.current) setPhase((p) => (p.at === "running" ? { ...p, job } : p));
      },
      (e: unknown) => {
        if (liveRef.current) setPhase({ at: "failed", message: errorText(t("scan.failed"), e) });
      },
    );
  };

  const cancel = () => {
    if (phase.at !== "running" || phase.job === null) return;
    setPhase({ ...phase, cancelling: true });
    void cancelJob(phase.job);
  };

  const scannerName = detected[choice]?.name ?? program.split(/[\\/]/).pop() ?? "";

  return (
    <Panel heading={t("scan.heading")} testId="scan-archive" className="scan-dialog" onClose={onClose}>
      {phase.at === "form" || phase.at === "failed" ? (
        <form
          className="scan-form"
          onSubmit={(e) => {
            e.preventDefault();
            start();
          }}
        >
          {loadError !== null && (
            <p role="alert" className="error">
              {loadError}
            </p>
          )}
          <label htmlFor={`${id}-found`}>{t("scan.found")}</label>
          <select
            id={`${id}-found`}
            data-testid="scan-found"
            value={String(choice)}
            onChange={(e) => choose(e.target.value)}
          >
            {detected.map((d, i) => (
              <option key={d.program} value={String(i)}>
                {d.name}
              </option>
            ))}
            <option value="-1">{detected.length === 0 ? t("scan.noneFound") : t("scan.other")}</option>
          </select>

          <div className="scan-row-head">
            <label htmlFor={`${id}-program`}>{t("scan.program")}</label>
            <button type="button" data-testid="scan-browse" onClick={browse}>
              {t("scan.browse")}
            </button>
          </div>
          <input
            id={`${id}-program`}
            data-testid="scan-program"
            type="text"
            spellCheck={false}
            autoComplete="off"
            value={program}
            onChange={(e) => {
              setProgram(e.target.value);
              setChoice(matchIndex(detected, { name: "", program: e.target.value, arguments: args }));
            }}
          />

          <label htmlFor={`${id}-args`}>{t("scan.arguments")}</label>
          <input
            id={`${id}-args`}
            data-testid="scan-arguments"
            type="text"
            spellCheck={false}
            autoComplete="off"
            aria-describedby={`${id}-hint`}
            value={args}
            onChange={(e) => setArgs(e.target.value)}
          />
          <p id={`${id}-hint`} className="note">
            {t("scan.argumentsHint")} {t("scan.how")}
          </p>
          {phase.at === "failed" && (
            <p role="alert" className="error" data-testid="scan-error">
              {phase.message}
            </p>
          )}
          <div className="actions dialog-actions">
            <button type="button" onClick={onClose}>
              {t("scan.cancel")}
            </button>
            <button type="submit" className="primary" data-testid="scan-start" disabled={program.trim() === ""}>
              {t("scan.ok")}
            </button>
          </div>
        </form>
      ) : phase.at === "running" ? (
        <div className="scan-running" data-testid="scan-running">
          <p role="status">
            {phase.cancelling
              ? t("scan.cancelling")
              : phase.scanning
                ? t("scan.scanning", { scanner: scannerName })
                : t("scan.extracting")}
          </p>
          <progress aria-label={t("scan.heading")} />
          <div className="actions dialog-actions">
            <button type="button" data-testid="scan-cancel" disabled={phase.job === null || phase.cancelling} onClick={cancel}>
              {t("scan.cancel")}
            </button>
          </div>
        </div>
      ) : phase.at === "cancelled" ? (
        <div className="scan-result">
          <p role="status">{t("scan.cancelled")}</p>
          <div className="actions dialog-actions">
            <button type="button" onClick={() => setPhase({ at: "form" })}>
              {t("scan.again")}
            </button>
            <button type="button" className="primary" onClick={onClose}>
              {t("scan.close")}
            </button>
          </div>
        </div>
      ) : (
        <ScanResult report={phase.report} onAgain={() => setPhase({ at: "form" })} onClose={onClose} />
      )}
    </Panel>
  );
}

function ScanResult({
  report: r,
  onAgain,
  onClose,
}: {
  report: ScanReportView;
  onAgain: () => void;
  onClose: () => void;
}) {
  const attention = r.verdict !== "none_reported" || r.not_scanned > 0;
  return (
    <div className="scan-result" data-testid="scan-result">
      <p role="status" className={attention ? "scan-verdict scan-attention" : "scan-verdict"} data-testid="scan-verdict">
        {verdictText(r)}
      </p>
      <p className="note" data-testid="scan-scope">
        {t("scan.scope", { files: formatNumber(r.files), folders: formatNumber(r.folders), seq: r.seq })}
      </p>
      {r.not_scanned > 0 && (
        <p className="scan-partial" data-testid="scan-partial">
          {t("scan.partial", { n: formatNumber(r.not_scanned) })}
        </p>
      )}
      <p className="note">{t("scan.limits")}</p>
      <details className="scan-output" open={r.verdict !== "none_reported"}>
        <summary>{t("scan.output")}</summary>
        {r.output_truncated && <p className="note">{t("scan.outputTruncated")}</p>}
        {r.output === "" ? (
          <p className="note">{t("scan.outputEmpty")}</p>
        ) : (
          <pre data-testid="scan-output">{r.output}</pre>
        )}
      </details>
      {r.cleanup_pending && <p className="note">{t("scan.cleanupPending")}</p>}
      <div className="actions dialog-actions">
        <button type="button" onClick={onAgain}>
          {t("scan.again")}
        </button>
        <button type="button" className="primary" data-testid="scan-close" onClick={onClose}>
          {t("scan.close")}
        </button>
      </div>
    </div>
  );
}
