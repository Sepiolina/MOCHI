# MOCHI desktop (Tauri v2)

**Not started; unblocked.** Stack decided (plan §9, O1): Tauri v2, React + TypeScript
(strict) on Vite, pnpm with a frozen lockfile, Node 24 LTS. Platforms (O11): Windows
10 22H2 / 11 and Ubuntu 22.04 / 24.04, x86-64. macOS is not supported in 1.0.

```text
apps/mochi-desktop/
├── src-tauri/          # Rust backend: thin commands over mochi-core
│   └── capabilities/   # least-privilege capability files
└── ui/                 # React + TypeScript + Vite
```

## Phase D0 checklist

1. Scaffold Tauri v2 with the React + TypeScript template; strict CSP (no remote content, no `eval`).
2. Add `apps/mochi-desktop/src-tauri` to the root `Cargo.toml` workspace `members`.
3. Typed IPC: generate TypeScript from Rust with `ts-rs`, wrap `invoke` in typed
   functions, and make CI fail when generated types are stale. (`tauri-specta` is
   deferred: its Tauri v2 line is still a release candidate.)
4. ESLint with `react/no-danger` as an error; Vitest + React Testing Library.
5. Job-runner bridge: `mochi-core` jobs → progress over a Tauri `Channel`,
   cancellation commands, CPU-bound work off the async runtime.
6. Exit: a sample job streams progress and cancels cleanly on Windows and Ubuntu,
   proven by a `tauri-driver` + WebdriverIO E2E test in CI.

## Rules that apply from the first commit (AGENTS.md, spec §23.3)

- No format logic in the UI or in Tauri commands. Commands validate input, call `mochi-core`, return typed results.
- Grant the webview no broad filesystem, shell, or HTTP permissions. Users pick paths
  through the dialog plugin; Rust does the I/O. Every new permission needs a written reason.
- Archive-supplied strings (names, labels, comments) render as text only: never
  `dangerouslySetInnerHTML`, never into `href`/`src`.
- No success styling for `UNKNOWN` / `OVERDUE` / `UNSUPPORTED` / `DEGRADED`.
  Never say "backed up", "safe", or "preserved" after a commit.
- Passphrases cross IPC once and are never persisted unless the user opts into the OS credential store.
- Foreign archives (D8): opened read-only, parsed in the helper process, extracted only
  through the shared restore engine, and tested with CRC wording, never MOCHI verification wording.
