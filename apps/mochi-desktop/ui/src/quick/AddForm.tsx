import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";

import { Hint } from "../a11y/Hint";
import { RECOVERY_CHOICES, recoveryLabel } from "../crypt/messages";
import { t } from "../i18n";
import { isIpcError } from "../ipc/jobs";
import { chooseQuickLocation, closeQuick, submitQuickAdd } from "../ipc/quick";
import type { ArchiveFormat } from "../ipc/quick";
import type { CompressionPreset } from "../ipc/write";
import { FormatSelect, LevelSelect } from "../write/FormatFields";
import { fitPreset, isMochi, nameForFormat, stripArchiveExtension } from "../write/format";

/** What the form starts with, from Rust: the suggested name, the folder, and the format. */
export interface AddFormStart {
  name: string;
  folder: string;
  format: ArchiveFormat;
}

/**
 * The form of the shell's "Add to archive…" (`--add`, owner decision 2026-10-20), in the
 * compact window: the archive's name, Browse…, the format, the compression level, and, for
 * `.mochi` only, a password and extra repair data. It names no path: the name is one file
 * name, the folder is shown as text, and Rust checks the request and refuses a name that
 * exists (the form stays open for another). Nothing is written until OK.
 */
export function AddForm({ start }: { start: AddFormStart }) {
  const [name, setName] = useState(start.name);
  const [folder, setFolder] = useState(start.folder);
  const [format, setFormat] = useState<ArchiveFormat>(start.format);
  const [level, setLevel] = useState<CompressionPreset>("balanced");
  const [encrypt, setEncrypt] = useState(false);
  const [recovery, setRecovery] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // The passphrase is read from these fields when OK is pressed and goes straight to Rust.
  const passRef = useRef<HTMLInputElement>(null);
  const againRef = useRef<HTMLInputElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);

  // The name is the first thing to type: focus it with the part before the extension selected.
  useEffect(() => {
    const input = nameRef.current;
    if (!input) return;
    input.focus();
    input.setSelectionRange(0, stripArchiveExtension(input.value).length);
  }, []);

  const mochi = isMochi(format);

  const chooseFormat = (next: ArchiveFormat) => {
    setFormat(next);
    setName((n) => nameForFormat(n, next));
    setLevel((l) => fitPreset(next, l));
    if (!isMochi(next)) {
      setEncrypt(false);
      setRecovery(null);
    }
  };

  const browse = () => {
    setError(null);
    chooseQuickLocation(name, format).then(
      (place) => {
        if (place === null) return; // the dialog was cancelled
        setFolder(place.folder.text);
        setName(place.name);
      },
      (e: unknown) => setError(isIpcError(e) ? e.message : t("quick.formFailed")),
    );
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    let passphrase: string | undefined;
    if (encrypt && mochi) {
      const first = passRef.current?.value ?? "";
      if (first === "") {
        setError(t("write.enterPassphrase"));
        return;
      }
      if (first !== (againRef.current?.value ?? "")) {
        setError(t("write.passphrasesDiffer"));
        return;
      }
      passphrase = first;
    }
    setBusy(true);
    const request = {
      name,
      format,
      level,
      ...(passphrase === undefined ? {} : { passphrase }),
      ...(recovery === null || !mochi ? {} : { recovery_percent: recovery }),
    };
    submitQuickAdd(request).then(
      () => undefined, // the job's own events take over this window
      (err: unknown) => {
        setBusy(false);
        setError(isIpcError(err) ? err.message : t("quick.formFailed"));
      },
    );
    // Rust holds it now; the page does not.
    if (passRef.current) passRef.current.value = "";
    if (againRef.current) againRef.current.value = "";
  };

  return (
    <form className="quick-form" data-testid="quick-form" onSubmit={submit}>
      <label>
        {t("quick.formName")}
        <span className="quick-row">
          <input
            ref={nameRef}
            type="text"
            value={name}
            spellCheck={false}
            autoComplete="off"
            data-testid="quick-name"
            disabled={busy}
            onChange={(e) => setName(e.target.value)}
          />
          <button type="button" data-testid="quick-browse" disabled={busy} onClick={browse}>
            {t("quick.formBrowse")}
          </button>
        </span>
      </label>
      <p className="note" data-testid="quick-folder" title={folder}>
        {t("quick.formFolder", { folder })}
      </p>
      <div className="quick-grid">
        <FormatSelect value={format} onChange={chooseFormat} disabled={busy} testId="quick-format" />
        <LevelSelect
          value={level}
          format={format}
          onChange={setLevel}
          disabled={busy}
          testId="quick-level"
          label={t("quick.formLevel")}
        />
      </div>
      {format === "tar" && (
        <p className="note" data-testid="quick-no-level">
          {t("quick.formLevelNone")}
        </p>
      )}
      {mochi ? (
        <fieldset disabled={busy} className="quick-mochi" data-testid="quick-mochi-options">
          <label className="quick-check">
            <input
              type="checkbox"
              checked={encrypt}
              data-testid="quick-encrypt"
              onChange={(e) => setEncrypt(e.target.checked)}
            />{" "}
            {t("quick.formEncrypt")}
          </label>
          {encrypt && (
            <div className="quick-grid" data-testid="quick-encrypt-fields">
              <label>
                {t("quick.formPassphrase")}
                <input ref={passRef} type="password" autoComplete="off" spellCheck={false} data-testid="quick-new-passphrase" />
              </label>
              <label>
                {t("quick.formRepeat")}
                <input
                  ref={againRef}
                  type="password"
                  autoComplete="off"
                  spellCheck={false}
                  data-testid="quick-new-passphrase-again"
                />
              </label>
            </div>
          )}
          <label>
            {t("quick.formRecovery")}{" "}
            <select
              data-testid="quick-recovery"
              value={recovery === null ? "" : String(recovery)}
              onChange={(e) => setRecovery(e.target.value === "" ? null : Number(e.target.value))}
            >
              <option value="">{t("write.recoveryNone")}</option>
              {RECOVERY_CHOICES.map((p) => (
                <option key={p} value={p}>
                  {recoveryLabel(p)}
                </option>
              ))}
            </select>
          </label>
        </fieldset>
      ) : (
        <Hint testId="quick-plain-note" text={t("quick.formPlain")} />
      )}
      {error && (
        <p role="alert" className="error" data-testid="quick-form-error">
          {error}
        </p>
      )}
      <div className="actions">
        <button type="submit" className="primary" data-testid="quick-ok" disabled={busy || name.trim() === ""}>
          {t("quick.formOk")}
        </button>
        <button
          type="button"
          data-testid="quick-form-cancel"
          disabled={busy}
          onClick={() => void closeQuick().catch(() => undefined)}
        >
          {t("common.cancel")}
        </button>
      </div>
    </form>
  );
}
