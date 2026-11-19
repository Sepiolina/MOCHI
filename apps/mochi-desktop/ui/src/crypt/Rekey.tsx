import { useEffect, useRef, useState } from "react";
import { Hint } from "../a11y/Hint";

import { useJobFocus } from "../a11y/focus";
import { Live } from "../a11y/Live";
import { Panel } from "../a11y/Panel";
import { errorText } from "../browser/format";
import { t } from "../i18n";
import type { ArchiveSummary } from "../ipc/browse";
import { listEnvelopes } from "../ipc/crypt";
import type { EnvelopeView } from "../ipc/crypt";
import { cancelJob, startJob } from "../ipc/jobs";
import type { JobEvent, JobOutcome, JobRequest } from "../ipc/jobs";
import { chooseDestination } from "../ipc/write";
import { EncryptionNotes, PassphraseForm } from "./PassphraseForm";
import { reencryptNote, removeNotRevocation, rekeyLines } from "./messages";

export interface RekeyProps {
  /** The open archive, if any. */
  archive: ArchiveSummary | null;
  /**
   * A rewrap committed: the archive as it now stands, or `null` when it could
   * not be opened again with the key held (the caller closes it).
   */
  onChanged: (archive: ArchiveSummary | null) => void;
}

type Mode = "closed" | "add" | "remove" | "reencrypt";
type Run = { state: "idle" } | { state: "running"; jobId: number | null } | { state: "done"; lines: string[] };

function outcomeLines(o: JobOutcome): string[] {
  switch (o.status) {
    case "completed":
      return o.rekey ? rekeyLines(o.rekey) : [t("common.completed", { summary: o.summary })];
    case "cancelled":
      return [t("rekey.cancelled")];
    case "failed":
      return [t("common.failed", { code: o.code, message: o.message }), t("rekey.failedNothing")];
    case "aborted":
      return [t("common.stoppedUnexpectedly", { message: o.message }), t("rekey.abortedHint")];
  }
}

/**
 * Plan D5 rekey (docs/dev/encrypted-profile.md): add a passphrase, remove one, or
 * re-encrypt to a new file. Everything is `mochi_core::rekey` in Rust. The
 * passphrase goes to Rust once, in the job request, and is not kept here.
 * Removal is not revocation, and the screen says so before it is done.
 */
export function Rekey({ archive, onChanged }: RekeyProps) {
  const [mode, setMode] = useState<Mode>("closed");
  const [envelopes, setEnvelopes] = useState<EnvelopeView[] | null>(null);
  const [run, setRun] = useState<Run>({ state: "idle" });
  const [error, setError] = useState<string | null>(null);
  const [destination, setDestination] = useState<string | null>(null);
  const changedRef = useRef(onChanged);
  useEffect(() => {
    changedRef.current = onChanged;
  });

  const handle = archive?.handle ?? null;
  const encrypted = archive?.profile.encrypted ?? false;

  const commits = archive?.commits.length ?? 0;
  useEffect(() => {
    if (handle === null || !encrypted) return;
    let live = true;
    listEnvelopes(handle).then(
      (l) => {
        if (live) setEnvelopes(l.envelopes);
      },
      (e: unknown) => {
        if (live) setError(errorText(t("rekey.listFailed"), e));
      },
    );
    return () => {
      live = false;
    };
  }, [handle, encrypted, commits]);

  const busy = run.state === "running";
  const rememberFocus = useJobFocus(busy);

  const start = (request: JobRequest) =>
    new Promise<void>((resolve) => {
      rememberFocus();
      setError(null);
      setRun({ state: "running", jobId: null });
      const onEvent = (event: JobEvent) => {
        if (event.event === "started") {
          setRun({ state: "running", jobId: event.job_id });
        } else if (event.event === "finished") {
          const o = event.outcome;
          setRun({ state: "done", lines: outcomeLines(o) });
          if (o.status === "completed" && o.rekey) {
            setMode("closed");
            if (o.rekey.action !== "reencrypted") changedRef.current(o.rekey.archive ?? null);
          }
          resolve();
        }
      };
      startJob(request, onEvent).catch((e: unknown) => {
        setRun({ state: "idle" });
        setError(errorText(t("rekey.startFailed"), e));
        resolve();
      });
    });

  const chooseFile = () => {
    setError(null);
    chooseDestination().then(
      (view) => {
        if (view === null) return;
        setDestination(view.destination);
        setMode("reencrypt");
      },
      (e: unknown) => setError(errorText(t("rekey.chooseFailed"), e)),
    );
  };

  const cancel = () => {
    if (run.state === "running" && run.jobId !== null) void cancelJob(run.jobId).catch(() => false);
  };

  return (
    <section aria-labelledby="rekey-heading" className="rekey" data-testid="rekey">
      <h2 id="rekey-heading" tabIndex={-1}>
        {t("rekey.heading")}
      </h2>
      {!encrypted ? (
        <p className="note" data-testid="rekey-unavailable">
          {archive ? t("rekey.notEncrypted") : t("rekey.openEncrypted")}
        </p>
      ) : (
        <>
          <ul aria-label={t("rekey.envelopes")} data-testid="envelopes">
            {(encrypted ? (envelopes ?? []) : []).map((e) => (
              <li key={e.envelope_id} data-testid="envelope">
                <span className="hash">{e.envelope_id}</span>
                {t("rekey.envelopeLine", { commit: e.written_by_commit, kdf: e.kdf })}
                {e.opened_with ? t("rekey.envelopeOpened") : ""}
              </li>
            ))}
          </ul>
          <div className="actions">
            <button type="button" data-testid="rekey-add" disabled={busy} onClick={() => setMode("add")}>
              {t("rekey.add")}
            </button>
            <button
              type="button"
              data-testid="rekey-remove"
              disabled={busy || (envelopes?.length ?? 0) < 2}
              title={(envelopes?.length ?? 0) < 2 ? t("rekey.removeLast") : undefined}
              onClick={() => setMode("remove")}
            >
              {t("rekey.remove")}
            </button>
            <button type="button" data-testid="rekey-reencrypt" disabled={busy} onClick={chooseFile}>
              {t("rekey.reencrypt")}
            </button>
          </div>

          {mode === "add" && archive && (
            <Panel
              label={t("rekey.addGroup")}
              heading={t("rekey.addGroup")}
              testId="rekey-panel"
              fallbackId="rekey-heading"
            >
              <PassphraseForm
                testId="rekey-add-form"
                label={t("rekey.newPassphrase")}
                confirm
                submitLabel={t("rekey.addSubmit")}
                onSubmit={(p) => start({ kind: "add_passphrase", handle: archive.handle, passphrase: p })}
                onCancel={() => setMode("closed")}
              >
                <EncryptionNotes creating={false} />
              </PassphraseForm>
            </Panel>
          )}

          {mode === "remove" && archive && envelopes && (
            <Panel
              label={t("rekey.removeGroup")}
              heading={t("rekey.removeGroup")}
              testId="rekey-panel"
              fallbackId="rekey-heading"
            >
              <p className="warning" data-testid="not-revocation">
                {removeNotRevocation()}
              </p>
              <Hint text={t("rekey.removeIntro")} />
              <ul>
                {envelopes.map((e) => (
                  <li key={e.envelope_id}>
                    <span className="hash">{e.envelope_id}</span>
                    {e.opened_with ? t("rekey.openedThis") : ""}{" "}
                    <button
                      type="button"
                      data-testid="remove-envelope"
                      disabled={busy}
                      aria-label={t("rekey.removeThisEnvelope", { id: e.envelope_id })}
                      onClick={() =>
                        void start({
                          kind: "remove_passphrase",
                          handle: archive.handle,
                          envelope_id: e.envelope_id,
                        })
                      }
                    >
                      {t("rekey.removeThis")}
                    </button>
                  </li>
                ))}
              </ul>
              <button type="button" onClick={() => setMode("closed")} disabled={busy}>
                {t("common.close")}
              </button>
            </Panel>
          )}

          {mode === "reencrypt" && archive && (
            <Panel
              label={t("rekey.reencryptGroup")}
              heading={t("rekey.reencryptHeading")}
              testId="rekey-panel"
              fallbackId="rekey-heading"
            >
              <p className="note" data-testid="reencrypt-note">
                {reencryptNote()}
              </p>
              {destination && <p>{t("rekey.newFile", { path: destination })}</p>}
              <PassphraseForm
                testId="reencrypt-form"
                label={t("rekey.newFilePassphrase")}
                confirm
                submitLabel={t("rekey.reencryptSubmit")}
                onSubmit={(p) => start({ kind: "reencrypt", handle: archive.handle, passphrase: p })}
                onCancel={() => setMode("closed")}
              >
                <EncryptionNotes creating={false} />
              </PassphraseForm>
            </Panel>
          )}
        </>
      )}
      {error && (
        <p role="alert" className="error" data-testid="rekey-error">
          {error}
        </p>
      )}
      {run.state === "running" && (
        <p data-testid="rekey-running">
          {t("rekey.working")}{" "}
          <button type="button" data-testid="rekey-cancel" onClick={cancel} disabled={run.jobId === null}>
            {t("common.cancel")}
          </button>
        </p>
      )}
      <Live data-testid="rekey-status">
        {run.state === "done" && run.lines.map((l) => <p key={l}>{l}</p>)}
      </Live>
    </section>
  );
}
