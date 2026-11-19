// Audit F8: Help, User guide calls the open command and nothing else, says the guide is for
// the command line, and reports a failure as text (no success styling).
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, describe, expect, test } from "vitest";

import { Help } from "./Help";
import { t } from "../i18n";

afterEach(() => clearMocks());

function backend(result: () => unknown) {
  const calls: { cmd: string; args: unknown }[] = [];
  mockIPC((cmd, args) => {
    calls.push({ cmd, args });
    if (cmd === "open_user_guide") return result();
    throw new Error(`unexpected command ${cmd}`);
  });
  return calls;
}

describe("Help", () => {
  test("labels the guide as written for the command line, before it is opened", () => {
    backend(() => null);
    render(<Help />);
    const button = screen.getByRole("button", { name: t("help.userGuide") });
    expect(button.textContent).toContain("command line");
    expect(button).toHaveAccessibleDescription(t("help.userGuideNote"));
    expect(screen.getByText(t("help.userGuideNote")).textContent).toContain("mochi command line");
  });

  test("the button calls the open command with no argument: the webview names no path", async () => {
    const calls = backend(() => null);
    render(<Help />);
    await userEvent.click(screen.getByRole("button", { name: t("help.userGuide") }));
    await waitFor(() => expect(screen.getByTestId("help-status").dataset.state).toBe("requested"));
    expect(calls.map((c) => c.cmd)).toEqual(["open_user_guide"]);
    expect(Object.keys((calls[0]?.args ?? {}) as object)).toEqual([]);
  });

  test("a failure is shown as text, with its code", async () => {
    backend(() => {
      throw { code: "IO_ERROR", message: "<b>The user guide is not part of this build.</b>" };
    });
    render(<Help />);
    await userEvent.click(screen.getByRole("button", { name: t("help.userGuide") }));
    const status = await screen.findByText(/IO_ERROR/);
    expect(status.dataset.state).toBe("failed");
    // Text, not markup.
    expect(status.querySelector("b")).toBeNull();
    expect(status.textContent).toContain("<b>The user guide");
  });
});
