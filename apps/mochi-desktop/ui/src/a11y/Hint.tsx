import { InfoIcon } from "../icons";

/**
 * A note the main path does not need to show: a small "i" whose tooltip holds
 * the words, which screen readers read as an ordinary note. The program shows
 * as little text as it can (owner request, 0.9.4); the full explanations are in
 * the user guide.
 */
export function Hint({ text, testId }: { text: string | readonly string[]; testId?: string }) {
  const lines = typeof text === "string" ? [text] : text;
  return (
    <span className="hint" role="note" data-testid={testId} title={lines.join("\n")}>
      <InfoIcon />
      <span className="sr-only">{lines.join(" ")}</span>
    </span>
  );
}
