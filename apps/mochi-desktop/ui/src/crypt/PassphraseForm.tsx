import { useEffect, useId, useRef, useState } from "react";
import type { FormEvent, ReactNode } from "react";

import { errorText } from "../browser/format";
import { t } from "../i18n";
import { isIpcError } from "../ipc/jobs";
import {
  dedupLeak as DEDUP,
  lostPassphrase as LOST,
  wrongPassphrase,
  wrongPassphraseNote,
} from "./messages";

export interface PassphraseFormProps {
  /** What the passphrase is for, e.g. "Passphrase for vault.mochi". */
  label: string;
  submitLabel: string;
  /** Ask twice and require the two to match (a passphrase being created). */
  confirm?: boolean;
  /** Notes shown with the field. */
  children?: ReactNode;
  /** Called once with the text; its rejection is shown and the form stays. */
  onSubmit: (passphrase: string) => Promise<unknown>;
  onCancel?: () => void;
  testId: string;
}

/**
 * Passphrase entry. The field is uncontrolled: the text lives in the DOM
 * input only until submit, is read once, is passed straight to `onSubmit`,
 * and the input is emptied at once. It is never put in React state, never
 * echoed, and never stored; the form shows only the error `onSubmit` returns.
 */
export function PassphraseForm(props: PassphraseFormProps) {
  const firstRef = useRef<HTMLInputElement>(null);
  const secondRef = useRef<HTMLInputElement>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const errorId = useId();

  // The fields are disabled while the passphrase is with Rust, which drops
  // keyboard focus; give it back to the first field when the answer is in.
  const wasBusyRef = useRef(false);
  useEffect(() => {
    if (wasBusyRef.current && !busy) firstRef.current?.focus();
    wasBusyRef.current = busy;
  }, [busy]);

  const clear = () => {
    if (firstRef.current) firstRef.current.value = "";
    if (secondRef.current) secondRef.current.value = "";
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const text = firstRef.current?.value ?? "";
    if (text === "") {
      setError(t("crypt.enterPassphrase"));
      return;
    }
    if (props.confirm && text !== (secondRef.current?.value ?? "")) {
      setError(t("crypt.passphrasesDiffer"));
      return;
    }
    setError(null);
    setBusy(true);
    clear();
    props.onSubmit(text).then(
      () => setBusy(false),
      (err: unknown) => {
        setBusy(false);
        // A wrong passphrase is an operational answer, never damage.
        setError(
          isIpcError(err) && err.code === "KEY_UNAVAILABLE"
            ? `${wrongPassphrase()} ${wrongPassphraseNote()}`
            : errorText(t("crypt.notDone"), err),
        );
      },
    );
  };

  return (
    <form onSubmit={submit} data-testid={props.testId} autoComplete="off">
      <label>
        {props.label}{" "}
        <input
          ref={firstRef}
          type="password"
          name="passphrase"
          autoComplete="off"
          data-testid={`${props.testId}-input`}
          aria-invalid={error !== null ? true : undefined}
          aria-describedby={error !== null ? errorId : undefined}
          disabled={busy}
          spellCheck={false}
        />
      </label>
      {props.confirm && (
        <label>
          {t("crypt.repeatIt")}{" "}
          <input
            ref={secondRef}
            type="password"
            name="passphrase-again"
            autoComplete="off"
            data-testid={`${props.testId}-confirm`}
            aria-describedby={error !== null ? errorId : undefined}
            disabled={busy}
            spellCheck={false}
          />
        </label>
      )}
      {props.children}
      <div className="actions">
        <button type="submit" data-testid={`${props.testId}-submit`} disabled={busy}>
          {props.submitLabel}
        </button>
        {props.onCancel && (
          <button type="button" data-testid={`${props.testId}-cancel`} onClick={props.onCancel} disabled={busy}>
            {t("common.cancel")}
          </button>
        )}
      </div>
      {error && (
        <p id={errorId} role="alert" className="error" data-testid={`${props.testId}-error`}>
          {error}
        </p>
      )}
    </form>
  );
}

/** The notes that go with any place a passphrase is chosen. */
export function EncryptionNotes({ creating }: { creating: boolean }) {
  return (
    <>
      <p className="warning" data-testid="lost-passphrase">
        {LOST()}
      </p>
      {creating && (
        <p className="note" data-testid="dedup-leak">
          {DEDUP()}
        </p>
      )}
    </>
  );
}
