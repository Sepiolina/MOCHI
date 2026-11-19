// MOCHI Find for many names at once (spec Annex B.2.21): the list, the options, what each name
// found (and "not found" said honestly while the search is incomplete), ticking matches and
// retrieving them. Archive-supplied names are text only; no path crosses from the window.
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { Channel } from "@tauri-apps/api/core";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test, vi } from "vitest";

import type { FindCoverageView, FindEvent, FindHit, FindQuery, FindResultView, FindScopeView, FindTermView } from "../ipc/find";
import type { JobEvent, JobRequest } from "../ipc/jobs";
import { Finder } from "./Finder";

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;
const hex = (s: string) => Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");

const HOSTILE = '<img src=x onerror="alert(1)">.pdf';

const hit = (archive: number, path: string, terms: number[], over: Partial<FindHit> = {}): FindHit => ({
  run: 1,
  archive,
  archive_path: `/data/arch${archive}.mochi`,
  archive_modified_ms: 1_700_000_000_000,
  path: { text: path, verbatim: true },
  terms,
  key: hex(path),
  kind: "file",
  size: "1000",
  ...over,
});

const coverage = (over: Partial<FindCoverageView> = {}): FindCoverageView => ({
  run: 1,
  state: "complete",
  complete: true,
  folders_visited: 1,
  folders_unreadable: 0,
  links_not_followed: 0,
  limit: null,
  archives_found: 2,
  archives_searched: 2,
  archives_pending: 0,
  archives_from_cache: 0,
  archives_skipped: 0,
  skipped: [],
  names_searched: 10,
  scopes: ["/data"],
  subfolders: true,
  failure: null,
  content_not_searched: true,
  ...over,
});

const term = (index: number, text: string, hits: number): FindTermView => ({
  index,
  text,
  hits,
  archives: hits > 0 ? 1 : 0,
  bytes: hits * 1000,
});
const page = (hits: FindHit[], over: Partial<FindResultView> = {}): FindResultView => ({
  hits,
  total_hits: hits.length,
  listed: hits.length,
  offset: 0,
  total_bytes: hits.length * 1000,
  archives_with_hits: new Set(hits.map((h) => h.archive)).size,
  terms: [],
  coverage: coverage(),
  ...over,
});
const scope = (index: number, path: string): FindScopeView => ({ index, path });

/** Three names asked: two found (one in two archives), one not. */
const THREE: FindResultView = page([hit(0, "docs/invoice.pdf", [0]), hit(1, "x/invoice.pdf", [0]), hit(1, HOSTILE, [1])], {
  terms: [term(0, "invoice.pdf", 2), term(1, "*.pdf", 1), term(2, "missing.doc", 0)],
});

/** `r` as Rust lists it for one name (`term`), or whole. */
function only(r: FindResultView, term: number | undefined): FindResultView {
  if (term === undefined) return r;
  const hits = r.hits.filter((h) => h.terms.includes(term));
  return { ...r, hits, listed: hits.length };
}

function backend(answer: (q: FindQuery) => FindResultView = (q) => only(THREE, q.term), cov: () => FindCoverageView = () => coverage()) {
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let scanChannel: Channel<FindEvent> | undefined;
  let jobChannel: Channel<JobEvent> | undefined;
  let n = 0;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    switch (cmd) {
      case "find_scopes":
        return [scope(0, "/data")];
      case "find_start":
        scanChannel = a.onEvent as Channel<FindEvent>;
        return 1;
      case "find_query":
        return page([], { coverage: cov() });
      case "find_query_many": {
        const q = a.query as FindQuery;
        return q.terms.length === 0 ? page([], { coverage: cov() }) : answer(q);
      }
      case "find_choose_retrieve_folder":
        return "/out/found";
      case "start_job":
        jobChannel = a.onEvent as Channel<JobEvent>;
        return 7;
      case "cancel_job":
        return true;
    }
    throw new Error(`unexpected command ${cmd}`);
  });
  const send = (channel: Channel<unknown> | undefined, message: unknown) => {
    if (!channel) throw new Error("no channel");
    const id = channel.id;
    act(() => internals().runCallback(id, { index: n++, message }));
  };
  return {
    calls,
    count: (cmd: string) => calls.filter((c) => c.cmd === cmd).length,
    last: (cmd: string) => calls.filter((c) => c.cmd === cmd).at(-1)?.args,
    scanEvent: (m: FindEvent) => send(scanChannel as Channel<unknown> | undefined, m),
    jobEvent: (m: JobEvent) => send(jobChannel as Channel<unknown> | undefined, m),
  };
}

afterEach(() => {
  clearMocks();
  vi.restoreAllMocks();
  localStorage.clear();
});

async function openList(user: ReturnType<typeof userEvent.setup>) {
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.click(screen.getByTestId("find-mode-list"));
  return screen.getByTestId("find-list-text");
}

test("a list of names is asked once, with what each name found and which were not found", async () => {
  const user = userEvent.setup();
  const b = backend();
  const box = await openList(user);
  expect(screen.getByTestId("find-mode-list")).toHaveAttribute("aria-pressed", "true");
  await user.type(box, "invoice.pdf{Enter}*.pdf, missing.doc;invoice.pdf");
  // The names as chips, with the repeat left out.
  expect(within(screen.getByTestId("find-chips")).getAllByRole("listitem").map((c) => c.textContent)).toEqual([
    "invoice.pdf",
    "*.pdf",
    "missing.doc",
  ]);
  expect(screen.getByTestId("find-list-count")).toHaveTextContent("3 names to find");
  expect(screen.getByTestId("find-list-left")).toHaveTextContent("1 repeated name left out");
  await waitFor(() =>
    expect(b.last("find_query_many")).toEqual({
      query: { terms: ["invoice.pdf", "*.pdf", "missing.doc"], match_case: false, whole_name: false, entries: "all", offset: 0, limit: 1000 },
    }),
  );
  expect(await screen.findByTestId("find-terms-score")).toHaveTextContent("2 of 3 names found");
  // Complete: "not found" may be said plainly.
  expect(screen.getByTestId("find-terms-missing")).toHaveTextContent(/^1 not found$/);
  const chips = screen.getAllByTestId("find-term");
  expect(chips.map((c) => c.textContent)).toEqual(["invoice.pdf2", "*.pdf1", "missing.docnot found"]);
  expect(chips[2]).toBeDisabled();
  expect(chips[2]).toHaveAttribute("data-found", "false");
  // The hostile name is text, and each row says which name it matched.
  expect(screen.getAllByTestId("find-hit")).toHaveLength(3);
  expect(screen.getByText(HOSTILE)).toBeInTheDocument();
  expect(document.querySelector("img")).toBeNull();
  // A row is tagged with the name it matched only when that says more than the file name.
  expect(screen.getAllByTestId("find-term-tag").map((x) => x.textContent)).toEqual(["*.pdf"]);
  // A name's chip shows only its matches; pressing it again shows all.
  await user.click(chips[1] as HTMLElement);
  await waitFor(() => expect(screen.getAllByTestId("find-hit")).toHaveLength(1));
  expect((b.last("find_query_many")?.query as FindQuery).term).toBe(1);
  expect(screen.getByTestId("find-term-showing")).toHaveTextContent("Showing the matches of *.pdf");
  await user.click(screen.getByTestId("find-term-all"));
  await waitFor(() => expect(screen.getAllByTestId("find-hit")).toHaveLength(3));
  // No path crosses from the window.
  for (const c of b.calls) expect(JSON.stringify(c.args)).not.toContain("/data");
});

test("while the search is incomplete a name is not found in what was searched, never simply not found", async () => {
  const user = userEvent.setup();
  const partial = coverage({ complete: false, archives_skipped: 1, archives_searched: 1 });
  backend((q) => ({ ...only(THREE, q.term), coverage: partial }), () => partial);
  const box = await openList(user);
  await user.type(box, "invoice.pdf{Enter}missing.doc");
  expect(await screen.findByTestId("find-terms-missing")).toHaveTextContent("1 not found in what was searched");
  expect(screen.getByTestId("find-incomplete")).toBeInTheDocument();
});

test("the names not found are copied, one per line", async () => {
  const user = userEvent.setup();
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  backend();
  const box = await openList(user);
  await user.type(box, "a{Enter}b");
  await user.click(await screen.findByTestId("find-copy-missing"));
  expect(writeText).toHaveBeenCalledWith("missing.doc");
});

test("a list pasted into the one-name box opens the list with it", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  const one = screen.getByTestId("find-query");
  fireEvent.paste(one, { clipboardData: { getData: () => "invoice.pdf\r\n*.pdf\r\nmissing.doc" } });
  expect(await screen.findByTestId("find-list-text")).toHaveValue("invoice.pdf\n*.pdf\nmissing.doc");
  expect(screen.getByTestId("find-notice")).toHaveTextContent("3 names pasted");
  await waitFor(() => expect(b.count("find_query_many")).toBeGreaterThan(0));
  // A single name pasted stays a single name.
  await user.click(screen.getByTestId("find-mode-one"));
  fireEvent.paste(screen.getByTestId("find-query"), { clipboardData: { getData: () => "report final.pdf" } });
  expect(screen.queryByTestId("find-list-text")).toBeNull();
});

test("one name with the default options is mochi find's query; any option asks with the options", async () => {
  const user = userEvent.setup();
  const b = backend();
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.type(screen.getByTestId("find-query"), "Report");
  await waitFor(() => expect(b.last("find_query")).toEqual({ text: "Report" }));
  expect(b.count("find_query_many")).toBe(0);
  await user.click(screen.getByTestId("find-match-case"));
  await waitFor(() =>
    expect(b.last("find_query_many")).toEqual({
      query: { terms: ["Report"], match_case: true, whole_name: false, entries: "all", offset: 0, limit: 1000 },
    }),
  );
  await user.click(screen.getByTestId("find-whole-name"));
  await user.selectOptions(screen.getByTestId("find-entries"), "files");
  await waitFor(() =>
    expect(b.last("find_query_many")).toEqual({
      query: { terms: ["Report"], match_case: true, whole_name: true, entries: "files", offset: 0, limit: 1000 },
    }),
  );
  // The choices are remembered for next time.
  expect(JSON.parse(localStorage.getItem("mochi.find.choices") ?? "{}")).toMatchObject({
    matchCase: true,
    wholeName: true,
    entries: "files",
  });
});

test("turning Search subfolders off scans again without them, and says so", async () => {
  const user = userEvent.setup();
  let sub = true;
  const b = backend(
    (q) => only(THREE, q.term),
    () => coverage({ subfolders: sub }),
  );
  render(<Finder active launchToken={0} onOpen={() => undefined} />);
  await screen.findAllByTestId("find-scope");
  await user.click(screen.getByTestId("find-scan"));
  await waitFor(() => expect(b.last("find_start")).toMatchObject({ fresh: false, subfolders: true }));
  b.scanEvent({ event: "started", run: 1 });
  b.scanEvent({ event: "finished", run: 1, state: "complete" });
  sub = false;
  await user.click(screen.getByTestId("find-subfolders"));
  await waitFor(() => expect(b.count("find_start")).toBe(2));
  expect(b.last("find_start")).toMatchObject({ subfolders: false });
  b.scanEvent({ event: "started", run: 2 });
  b.scanEvent({ event: "finished", run: 2, state: "complete" });
  expect(await screen.findByTestId("find-subfolders-off")).toHaveTextContent("Subfolders were not searched");
});

test("ticked matches are retrieved through a job that names no path, and the report is text", async () => {
  const user = userEvent.setup();
  const b = backend();
  const box = await openList(user);
  await user.type(box, "invoice.pdf{Enter}*.pdf");
  await screen.findAllByTestId("find-hit");
  expect(screen.getByTestId("find-retrieve")).toBeDisabled();
  expect(screen.getByTestId("find-selection")).toHaveTextContent("Tick the matches to retrieve");

  // Tick one by its box and one with Space on the row; then all with the header box.
  const checks = screen.getAllByTestId("find-hit-check");
  await user.click(checks[0] as HTMLElement);
  (screen.getAllByTestId("find-hit")[2] as HTMLElement).focus();
  await user.keyboard(" ");
  expect(screen.getByTestId("find-selection")).toHaveTextContent("2 selected · 1.9 KiB");
  expect(screen.getByTestId("find-selection")).toHaveTextContent("from 2 archives");
  await user.click(screen.getByTestId("find-select-all"));
  expect(screen.getAllByTestId("find-hit-check").every((c) => (c as HTMLInputElement).checked)).toBe(true);

  await user.click(screen.getByTestId("find-retrieve"));
  const dialog = await screen.findByTestId("find-retrieve-dialog");
  expect(within(dialog).getByTestId("find-retrieve-what")).toHaveTextContent("Files: 3 · Folders: 0 · From archives: 2");
  // Nothing starts until a folder is chosen (in Rust's dialog).
  expect(within(dialog).getByTestId("find-retrieve-ok")).toBeDisabled();
  await user.click(within(dialog).getByTestId("find-retrieve-choose"));
  expect(await within(dialog).findByTestId("find-retrieve-folder")).toHaveTextContent("/out/found");
  await user.click(within(dialog).getByTestId("find-retrieve-together"));
  await user.click(within(dialog).getByTestId("find-retrieve-replace"));
  await user.click(within(dialog).getByTestId("find-retrieve-ok"));

  await waitFor(() => expect(b.count("start_job")).toBe(1));
  const request = b.last("start_job")?.request as JobRequest;
  expect(request).toEqual({
    kind: "retrieve_found",
    run: 1,
    items: [
      { archive: 0, key: hex("docs/invoice.pdf") },
      { archive: 1, key: hex("x/invoice.pdf") },
      { archive: 1, key: hex(HOSTILE) },
    ],
    layout: "together",
    on_conflict: "replace",
  });
  expect(JSON.stringify(request)).not.toContain("/out");

  b.jobEvent({ event: "started", job_id: 7 });
  b.jobEvent({ event: "progress", job_id: 7, phase: "restore", completed: 1, total: 3 });
  expect(await screen.findByTestId("find-retrieve-running")).toBeInTheDocument();
  b.jobEvent({
    event: "finished",
    job_id: 7,
    outcome: {
      status: "completed",
      summary: "retrieved 2 files from 2 archives to /out/found; 1 not extracted",
      retrieve: {
        destination: "/out/found",
        layout: "together",
        files: 2,
        directories: 1,
        bytes: 2000,
        not_extracted_total: 1,
        archives_not_done: 1,
        archives: [
          {
            archive_path: "/data/arch0.mochi",
            folder: "/out/found",
            state: "extracted",
            message: null,
            asked: 1,
            files: 2,
            directories: 1,
            bytes: 2000,
            not_extracted: [
              { path: { text: HOSTILE, verbatim: true }, code: "NAME_COLLISION", reason: { text: "already there", verbatim: true } },
            ],
            not_extracted_total: 1,
          },
          {
            archive_path: "/data/arch1.mochi",
            folder: null,
            state: "changed",
            message: { text: "this archive changed after it was scanned", verbatim: true },
            asked: 2,
            files: 0,
            directories: 0,
            bytes: 0,
            not_extracted: [],
            not_extracted_total: 0,
          },
        ],
        files_not_flushed: false,
        opened_folder: false,
      },
    },
  });
  const report = await screen.findByTestId("find-retrieve-report");
  expect(within(report).getByTestId("find-retrieve-summary")).toHaveTextContent("Retrieved 2 files (1.9 KiB)");
  expect(within(report).getByTestId("find-retrieve-skipped")).toHaveTextContent("1 entry was not retrieved");
  expect(within(report).getByTestId("find-retrieve-not-done")).toHaveTextContent("1 archive could not be used");
  const rows = within(report).getAllByTestId("find-retrieve-archive");
  expect(rows.map((r) => r.getAttribute("data-state"))).toEqual(["extracted", "changed"]);
  expect(rows[1]).toHaveTextContent("Changed since the scan");
  expect(within(report).getByText(HOSTILE)).toBeInTheDocument();
  expect(document.querySelector("img")).toBeNull();
  // Never verification or "safe" wording (spec §23.3).
  expect(report.textContent ?? "").not.toMatch(/verified|safe|backed up|preserved/i);
  await user.click(screen.getByTestId("find-retrieve-close"));
  expect(screen.queryByTestId("find-retrieve-dialog")).toBeNull();
});

test("a retrieve that is stopped says what stays", async () => {
  const user = userEvent.setup();
  const b = backend();
  const box = await openList(user);
  await user.type(box, "invoice.pdf{Enter}*.pdf");
  await screen.findAllByTestId("find-hit");
  await user.click(screen.getByTestId("find-select-all"));
  await user.click(screen.getByTestId("find-retrieve"));
  await user.click(await screen.findByTestId("find-retrieve-choose"));
  await screen.findByText("/out/found");
  await user.click(screen.getByTestId("find-retrieve-ok"));
  await waitFor(() => expect(b.count("start_job")).toBe(1));
  b.jobEvent({ event: "started", job_id: 7 });
  await user.click(await screen.findByTestId("find-retrieve-stop"));
  expect(b.last("cancel_job")).toEqual({ jobId: 7 });
  b.jobEvent({ event: "finished", job_id: 7, outcome: { status: "cancelled" } });
  expect(await screen.findByTestId("find-retrieve-outcome")).toHaveTextContent("Files retrieved before that stay");
  await user.click(screen.getByTestId("find-retrieve-again"));
  expect(screen.getByTestId("find-retrieve-ok")).toBeEnabled();
});

/** 2,500 log files across two archives, served a page at a time as Rust does. */
function bigLogs(q: FindQuery): FindResultView {
  const all = Array.from({ length: 2_500 }, (_, i) => hit(i < 1_500 ? 0 : 1, `logs/app-${String(i).padStart(5, "0")}.log`, [0]));
  const offset = q.offset ?? 0;
  const limit = q.limit ?? 1_000;
  return page(all.slice(offset, offset + limit), {
    total_hits: 2_500,
    listed: 2_500,
    offset,
    total_bytes: 2_500_000,
    archives_with_hits: 2,
    terms: [term(0, "*.log", 2_500), term(1, "nothing.txt", 0)],
  });
}

test("every match can be reached: the list draws the rows in view and fetches their page", async () => {
  const user = userEvent.setup();
  const b = backend(bigLogs);
  const box = await openList(user);
  await user.type(box, "*.log{Enter}nothing.txt");
  await waitFor(() => expect(screen.getAllByTestId("find-hit").length).toBeGreaterThan(0));
  expect(screen.getByTestId("find-status")).toHaveTextContent(/^2,500 matches$/);
  // Only the rows in view are in the document, not 1,000 or 2,500.
  expect(screen.getAllByTestId("find-hit").length).toBeLessThan(100);
  const pagesAsked = () => b.calls.filter((c) => c.cmd === "find_query_many" && (c.args.query as FindQuery).offset !== 0);
  expect(pagesAsked()).toHaveLength(0);
  // Scrolling far down asks for the page there, and draws its rows.
  const box2 = screen.getByTestId("find-results");
  box2.scrollTop = 2_000 * 30;
  fireEvent.scroll(box2);
  await waitFor(() => expect(screen.getByText("app-02000.log")).toBeInTheDocument());
  // The page there, and the one just above it for the rows drawn beyond the view.
  expect(pagesAsked().map((c) => (c.args.query as FindQuery).offset)).toEqual([1_000, 2_000]);
  expect(screen.queryByText("app-00000.log")).toBeNull();
  // End goes to the last match, loading its page.
  const row = screen.getAllByTestId("find-hit")[0] as HTMLElement;
  row.focus();
  await user.keyboard("{End}");
  await waitFor(() => expect(screen.getByText("app-02499.log").closest("button")).toHaveFocus());
});

test("all matches can be ticked and retrieved without listing them", async () => {
  const user = userEvent.setup();
  const b = backend(bigLogs);
  const box = await openList(user);
  await user.type(box, "*.log{Enter}nothing.txt");
  await waitFor(() => expect(screen.getAllByTestId("find-hit").length).toBeGreaterThan(0));
  await user.click(screen.getByTestId("find-select-all"));
  expect(screen.getByTestId("find-tick-all")).toHaveTextContent("All 1,000 matches loaded so far are ticked.");
  await user.click(screen.getByTestId("find-tick-all-button"));
  expect(screen.getByTestId("find-tick-all")).toHaveTextContent("All 2,500 matches are ticked");
  expect(screen.getByTestId("find-selection")).toHaveTextContent("2,500 selected");
  expect(screen.getByTestId("find-selection")).toHaveTextContent("from 2 archives");
  await user.click(screen.getByTestId("find-retrieve"));
  const dialog = await screen.findByTestId("find-retrieve-dialog");
  expect(within(dialog).getByTestId("find-retrieve-what")).toHaveTextContent("Matches: 2,500 · From archives: 2");
  await user.click(within(dialog).getByTestId("find-retrieve-choose"));
  await within(dialog).findByText("/out/found");
  await user.click(within(dialog).getByTestId("find-retrieve-ok"));
  await waitFor(() => expect(b.count("start_job")).toBe(1));
  expect(b.last("start_job")?.request).toEqual({
    kind: "retrieve_found",
    run: 1,
    items: [],
    all: { terms: ["*.log", "nothing.txt"], match_case: false, whole_name: false, entries: "all" },
    layout: "per_archive",
    on_conflict: "add_new",
  });
});

test("unticking one row after ticking all leaves the other loaded rows ticked", async () => {
  const user = userEvent.setup();
  backend(bigLogs);
  const box = await openList(user);
  await user.type(box, "*.log{Enter}nothing.txt");
  await waitFor(() => expect(screen.getAllByTestId("find-hit").length).toBeGreaterThan(0));
  await user.click(screen.getByTestId("find-select-all"));
  await user.click(screen.getByTestId("find-tick-all-button"));
  await user.click(screen.getAllByTestId("find-hit-check")[0] as HTMLElement);
  expect(screen.getByTestId("find-selection")).toHaveTextContent("999 selected");
  expect(screen.queryByTestId("find-tick-all")).toBeNull();
});
