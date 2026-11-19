import { useId } from "react";

import { isLocale, LOCALES, setLocale, t, useLocale } from "../i18n";
import { SettingGroup, SettingRow } from "./Row";

/**
 * The display-language selector. Each language is shown in its own name (not
 * translated), so it can be found whichever language is on. The choice applies
 * at once and is remembered on this computer (see `i18n/index.ts`).
 */
export function Language() {
  const locale = useLocale();
  const id = useId();
  const noteId = useId();
  return (
    <SettingGroup>
      <SettingRow
        title={t("settings.languageLabel")}
        titleFor={id}
        description={t("settings.languageShort")}
        descriptionId={noteId}
        more={t("settings.languageNote")}
      >
        <select
          id={id}
          data-testid="language-select"
          aria-describedby={noteId}
          value={locale}
          onChange={(e) => {
            if (isLocale(e.target.value)) setLocale(e.target.value);
          }}
        >
          {LOCALES.map((l) => (
            <option key={l.id} value={l.id} lang={l.id}>
              {l.name}
            </option>
          ))}
        </select>
      </SettingRow>
    </SettingGroup>
  );
}
