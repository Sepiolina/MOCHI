import { t } from "../i18n";
import type { NameText } from "../ipc/browse";

/**
 * An archive-supplied string, as text only (spec §23.3 #9). React renders it
 * as a text node, so markup in a name stays inert. The backend has already
 * escaped control and bidi characters and bytes that are not UTF-8; when it
 * did, a marker says the shown text is not the stored name.
 */
export function Name({ name }: { name: NameText }) {
  return (
    <>
      <span className="name">{name.text}</span>
      {!name.verbatim && (
        <span className="escaped" title={t("common.escapedHint")}>
          {t("common.escapedMarker")}
        </span>
      )}
    </>
  );
}
