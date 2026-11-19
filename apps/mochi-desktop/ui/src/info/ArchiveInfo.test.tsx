// The archive Info window (owner request 2026-11-14): totals, sizes and ratio as Rust
// computed them, the recorded settings, and plain words for what the format does not have.
import { act, render, screen, waitFor } from "@testing-library/react";
import type { Channel } from "@tauri-apps/api/core";
import userEvent from "@testing-library/user-event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, expect, test, vi } from "vitest";

import type { ArchiveInfoView, ArchiveSummary } from "../ipc/browse";
import type { JobEvent } from "../ipc/jobs";
import { audit } from "../test/audit";
import { ArchiveInfo, normalizeComment, ratioPercent } from "./ArchiveInfo";

const archive: ArchiveSummary = {
  handle: 3,
  file_name: "photos <b>2026</b>.mochi",
  archive_id: "ab".repeat(16),
  profile: { tar_compatible: false, encrypted: true, redundancy_percent: 10 },
  commits: [0, 1, 2].map((seq) => ({
    seq,
    commit_id: "c".repeat(64),
    time: null,
    checkpoint: true,
    retained: true,
    expired: false,
    holds: [],
  })),
  retention_error: null,
};

const view = (over: Partial<ArchiveInfoView> = {}): ArchiveInfoView => ({
  seq: 2,
  folders: "18",
  files: "52",
  total_size: "1370125",
  packed_size: "907505",
  chunks: "60",
  file_size: "1000000",
  chunk_size: "8388608",
  zstd_level: 3,
  comment: null,
  ...over,
});

afterEach(() => clearMocks());

test("ratio is packed over total, rounded, and absent when nothing is stored", () => {
  expect(ratioPercent("907505", "1370125")).toBe(66);
  expect(ratioPercent("1", "3")).toBe(33);
  expect(ratioPercent("2", "3")).toBe(67);
  expect(ratioPercent("0", "0")).toBeNull();
  expect(ratioPercent("x", "3")).toBeNull();
  expect(ratioPercent("18446744073709551615", "18446744073709551615")).toBe(100);
});

test("shows the version's totals and settings, and asks Rust for the version on screen", async () => {
  const calls: unknown[] = [];
  mockIPC((cmd, args) => {
    if (cmd === "archive_info") {
      calls.push(args);
      return view();
    }
    throw new Error(cmd);
  });
  const onClose = vi.fn();
  render(<ArchiveInfo archive={archive} seq={2} onClose={onClose} />);
  expect(await screen.findByTestId("info-files")).toHaveTextContent("52");
  expect(calls).toEqual([{ handle: 3, seq: 2 }]);
  expect(screen.getByTestId("info-folders")).toHaveTextContent("18");
  expect(screen.getByTestId("info-total")).toHaveTextContent("1.3 MiB");
  expect(screen.getByTestId("info-total")).toHaveAttribute("title", expect.stringContaining("1,370,125 bytes"));
  expect(screen.getByTestId("info-packed")).toHaveTextContent("886.2 KiB");
  expect(screen.getByTestId("info-ratio")).toHaveTextContent("66%");
  expect(screen.getByTestId("info-dictionary")).toHaveTextContent("Up to 8.0 MiB");
  expect(screen.getByTestId("info-level")).toHaveTextContent("Zstandard level 3");
  expect(screen.getByTestId("info-encryption")).toHaveTextContent("Present");
  expect(screen.getByTestId("info-recovery")).toHaveTextContent("10% parity");
  // The fixture archive is encrypted: it cannot have a comment, and says why.
  expect(screen.getByTestId("info-comment-text")).toHaveTextContent("cannot have a comment");
  // The archive's file name is text, never markup.
  expect(screen.getByTestId("archive-info").querySelector("b")).toBeNull();
  expect(screen.getByText("photos <b>2026</b>.mochi")).toBeInTheDocument();
  expect(audit()).toEqual([]);
  await userEvent.click(screen.getByTestId("archive-info-ok"));
  expect(onClose).toHaveBeenCalledOnce();
});

test("what the archive did not record is said, never guessed; an empty version has no ratio", async () => {
  mockIPC(() => view({ total_size: "0", packed_size: "0", chunk_size: null, zstd_level: null, file_size: null }));
  render(<ArchiveInfo archive={{ ...archive, profile: { ...archive.profile, encrypted: false, redundancy_percent: null } }} seq={0} onClose={() => {}} />);
  expect(await screen.findByTestId("info-ratio")).toHaveTextContent("—");
  expect(screen.getByTestId("info-dictionary")).toHaveTextContent("Not recorded");
  expect(screen.getByTestId("info-level")).toHaveTextContent("Not recorded");
  expect(screen.queryByTestId("info-file-size")).toBeNull();
  expect(screen.getByTestId("info-encryption")).toHaveTextContent("Absent");
  expect(screen.getByTestId("info-recovery")).toHaveTextContent("Absent");
});

test("a failure is said with its code", async () => {
  mockIPC(() => {
    throw { code: "INVALID_ARGUMENT", message: "no such archive" };
  });
  render(<ArchiveInfo archive={archive} seq={2} onClose={() => {}} />);
  await waitFor(() => expect(screen.getByTestId("archive-info-error")).toHaveTextContent("INVALID_ARGUMENT"));
});

type Internals = { runCallback: (id: number, data: unknown) => void };
const internals = () => (window as unknown as { __TAURI_INTERNALS__: Internals }).__TAURI_INTERNALS__;
const plain: ArchiveSummary = { ...archive, profile: { tar_compatible: false, encrypted: false, redundancy_percent: null } };

test("a comment is archive text: shown exactly, line breaks kept, markup inert", async () => {
  const hostile = 'Trip <img src=x onerror="alert(1)">\n\tday two';
  mockIPC(() => view({ comment: hostile }));
  render(<ArchiveInfo archive={plain} seq={2} onClose={() => {}} onCommitted={() => {}} />);
  const text = await screen.findByTestId("info-comment-text");
  expect(text.textContent).toBe(hostile);
  expect(document.querySelector("img")).toBeNull();
  expect(audit()).toEqual([]);
});

test("Line ends become LF and an empty comment means none", () => {
  expect(normalizeComment("a\r\nb\rc")).toBe("a\nb\nc");
  expect(normalizeComment("")).toBeNull();
});

test("on the latest version the comment is edited and saved as a new version", async () => {
  const user = userEvent.setup();
  const calls: { cmd: string; args: Record<string, unknown> }[] = [];
  let channel: Channel<JobEvent> | undefined;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args: a });
    if (cmd === "archive_info") return view({ comment: "old" });
    if (cmd === "start_job") {
      channel = a.onEvent as Channel<JobEvent>;
      return 5;
    }
    throw new Error(cmd);
  });
  const committed = vi.fn();
  render(<ArchiveInfo archive={plain} seq={2} onClose={() => {}} onCommitted={committed} />);
  await user.click(await screen.findByTestId("info-comment-edit"));
  const box = screen.getByTestId("info-comment-input");
  expect(box).toHaveValue("old");
  await user.clear(box);
  await user.type(box, "new text");
  expect(screen.getByTestId("info-comment-count")).toHaveTextContent("8 of 16,384 bytes");
  expect(audit()).toEqual([]);
  await user.click(screen.getByTestId("info-comment-save"));
  await waitFor(() => expect(channel).toBeDefined());
  expect(calls.find((c) => c.cmd === "start_job")?.args.request).toEqual({
    kind: "set_archive_comment",
    handle: 3,
    comment: "new text",
  });
  const first = plain.commits[0];
  if (!first || !channel) throw new Error("no job started");
  const after = { ...plain, commits: [...plain.commits, { ...first, seq: 3 }] };
  const id = channel.id;
  act(() =>
    internals().runCallback(id, {
      index: 0,
      message: {
        event: "finished",
        job_id: 5,
        outcome: {
          status: "completed",
          summary: "",
          commit: {
            operation: "comment",
            status: "LOCAL_COMMITTED",
            seq: 3,
            commit_id: "d".repeat(64),
            files: 0,
            directories: 0,
            bytes: "0",
            removed: 0,
            skipped: [],
            durability_note: null,
            archive: after,
            reopen_error: null,
          },
        },
      },
    }),
  );
  expect(committed).toHaveBeenCalledWith(after);
});

test("an empty comment is saved as no comment, and a refusal is shown with its code", async () => {
  const user = userEvent.setup();
  let channel: Channel<JobEvent> | undefined;
  const requests: unknown[] = [];
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (cmd === "archive_info") return view({ comment: "old" });
    if (cmd === "start_job") {
      requests.push(a.request);
      channel = a.onEvent as Channel<JobEvent>;
      return 6;
    }
    throw new Error(cmd);
  });
  render(<ArchiveInfo archive={plain} seq={2} onClose={() => {}} onCommitted={() => {}} />);
  await user.click(await screen.findByTestId("info-comment-edit"));
  await user.clear(screen.getByTestId("info-comment-input"));
  await user.click(screen.getByTestId("info-comment-save"));
  await waitFor(() => expect(requests).toEqual([{ kind: "set_archive_comment", handle: 3, comment: null }]));
  if (!channel) throw new Error("no job started");
  const id = channel.id;
  act(() =>
    internals().runCallback(id, {
      index: 0,
      message: { event: "finished", job_id: 6, outcome: { status: "failed", code: "WRITER_POISONED", message: "x" } },
    }),
  );
  expect(await screen.findByTestId("info-comment-error")).toHaveTextContent("WRITER_POISONED");
});

test("an earlier version and an encrypted archive cannot be edited, and say why", async () => {
  mockIPC(() => view({ comment: "kept" }));
  const { unmount } = render(<ArchiveInfo archive={plain} seq={1} onClose={() => {}} onCommitted={() => {}} />);
  expect(await screen.findByTestId("info-comment-text")).toHaveTextContent("kept");
  expect(screen.queryByTestId("info-comment-edit")).toBeNull();
  expect(screen.getByText(/earlier version/)).toBeInTheDocument();
  unmount();
  mockIPC(() => view({ comment: null }));
  render(<ArchiveInfo archive={archive} seq={2} onClose={() => {}} onCommitted={() => {}} />);
  expect(await screen.findByTestId("info-comment-text")).toHaveTextContent("cannot have a comment");
  expect(screen.queryByTestId("info-comment-edit")).toBeNull();
});
