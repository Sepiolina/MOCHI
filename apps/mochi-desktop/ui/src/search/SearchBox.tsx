import { useEffect, useRef, useState } from "react";

import { CloseIcon, SearchIcon } from "../icons";
import { t } from "../i18n";

/** How long typing pauses before the search runs, in milliseconds. */
export const SEARCH_DELAY = 300;

/**
 * The toolbar's search field. It finds names in the archive on screen (MOCHI,
 * ZIP, 7z, RAR, tar…), never file contents; the browser lists the matches in
 * place of the folder. Enter searches at once, Escape clears.
 */
export function SearchBox({
  disabled,
  onQuery,
  clearToken,
}: {
  disabled: boolean;
  onQuery: (query: string) => void;
  /** Changes when the search should be emptied (another archive, a hit opened). */
  clearToken: number;
}) {
  const [value, setValue] = useState("");
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onQueryRef = useRef(onQuery);
  useEffect(() => {
    onQueryRef.current = onQuery;
  });

  const [seen, setSeen] = useState(clearToken);
  if (seen !== clearToken) {
    setSeen(clearToken);
    setValue("");
  }
  useEffect(() => {
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = null;
    onQueryRef.current("");
  }, [clearToken]);

  const push = (v: string, now: boolean) => {
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = null;
    if (now) onQueryRef.current(v.trim());
    else timerRef.current = setTimeout(() => onQueryRef.current(v.trim()), SEARCH_DELAY);
  };
  useEffect(
    () => () => {
      if (timerRef.current) clearTimeout(timerRef.current);
    },
    [],
  );

  return (
    <search className="searchbox" data-disabled={disabled || undefined}>
      <SearchIcon />
      <input
        type="search"
        data-testid="search-query"
        aria-label={t("search.label")}
        placeholder={t("search.placeholder")}
        value={value}
        disabled={disabled}
        spellCheck={false}
        autoComplete="off"
        onChange={(e) => {
          setValue(e.target.value);
          push(e.target.value, false);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            push(value, true);
          } else if (e.key === "Escape" && value !== "") {
            e.preventDefault();
            setValue("");
            push("", true);
          }
        }}
      />
      {value !== "" && (
        <button
          type="button"
          className="searchbox-clear"
          data-testid="search-clear"
          aria-label={t("search.clear")}
          title={t("search.clear")}
          onClick={() => {
            setValue("");
            push("", true);
          }}
        >
          <CloseIcon />
        </button>
      )}
    </search>
  );
}
