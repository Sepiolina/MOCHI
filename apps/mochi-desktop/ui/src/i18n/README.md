# Languages

Shipped: English (`en`), Thai (`th`), Simplified Chinese (`zh-CN`).

```
en/<screen>.ts      the source: keys and English wording (defines the types)
th/<screen>.ts      Thai, same file names, same exported const names
zh-CN/<screen>.ts   Simplified Chinese
en.ts th.ts zh-CN.ts  assemble one file per screen into one catalogue
index.ts            t(), tn(), tc(), formatNumber(), setLocale(), useLocale()
```

## Translating a file

1. Open `th/<screen>.ts` (or `zh-CN/`). The header says `TODO(translate)`; delete that line when the file is done.
2. Replace each **value**. Never change a key.
3. Keep every `{placeholder}` exactly as written (same names, any position that suits the grammar). Do not add or drop one.
4. Run `pnpm typecheck && pnpm test` in `apps/mochi-desktop/ui`. A missing or extra key fails `tsc`; a different placeholder set, or a banned word, fails `i18n.test.ts`.

A key that has an `...One` and `...Many` form (`tn`, `tc`) is chosen by `n === 1`. Thai and Chinese have no plural: put the same wording in both.

## Translation rules

- **Spec section 23.3 wording applies in every language.** Never say an archive is backed up, safe, secure or preserved; never say a passphrase was revoked (removing one is not revocation); never say something was repaired, or recovered completely, unless the report says so. A deletion removes the file from the current snapshot, not from history. Only a PASS may sound like success; UNKNOWN, OVERDUE, UNSUPPORTED and DEGRADED never do. Partial salvage is called partial. `i18n.test.ts` holds a banned-word list per language as a backstop; it is not the whole rule, so when in doubt, say less.
- **Never translate archive-supplied text.** File names, labels and foreign-format messages are passed in as parameters and shown as they are.
- **Keep identifiers in English:** error codes (`KEY_UNAVAILABLE`, ...), status values (`PASS`, `FAIL`, `DEGRADED`, `UNKNOWN`, `OVERDUE`, `UNSUPPORTED`), `LOCAL_COMMITTED`, command and file-format names (`.mochi`, ZIP, tar, BLAKE3, ...), key names in the user guide. They are stable and are what a person searches for or reports. Explain them in the local language around them instead.
- **A placeholder carries archive data, a number, or the result of another lookup, never a word.** `{them}` filled with "it"/"them" cannot be translated; write two full sentences (`write.committedDeleteOne` / `...Many`) instead of composing fragments, since word order and particles differ. A closed backend vocabulary (`not_needed`, `copy_confirmed`) is a catalogue group read with `tCode("check.enum.step", code)`; a job phase is read with `phaseLabel(phase)`. `i18n.test.ts` fails on a quoted word inside a `t()` parameter list.
- **Native dialogs** (open, save, folder) are Rust's. Their titles and file-type labels are `dialog.*` keys; `ipc/dialogs.ts` builds the `DialogText` that every picker command receives. Rust accepts only short plain strings (120 characters, no control characters) and keeps its English default otherwise. File extensions and default file names are not translated.
- Do not add English text to components; use `t("area.key")`. ESLint refuses text in JSX.
- Wording that is function-of-language lives in functions (`checkNote()`), never in a constant evaluated at import: the language can change while the app runs.

## Adding a language

1. Add the tag to `Locale`, `LOCALES` (the language's own name), `isLocale`, and `CATALOGUES` in `index.ts`; map OS language tags in `localeFromLanguageTags`.
2. Copy `en/` to `<tag>/` and `en.ts` to `<tag>.ts` (type each file with `satisfies Catalogue<typeof enScreen>`).
3. Add a banned-word list for it in `i18n.test.ts` (`BANNED`).
4. Check fonts in `styles.css` (`:root:lang(<tag>)`): system fonts only, the CSP forbids remote ones.
5. The Explorer menu text is separate (`crates/mochi-shellext/src/plan.rs`, `Lang`).

The language is chosen from the saved choice (`localStorage` key `mochi.language`), else the webview language. Settings has the selector; changing it re-renders in place.
