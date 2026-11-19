import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";

import { ResultBar } from "./Result";
import type { ResultDetails } from "./details";

const clean: ResultDetails = { rows: [{ label: "Files", value: "1" }], notes: [] };
const caution = (n: number): ResultDetails => ({
  rows: Array.from({ length: n }, (_, i) => ({ label: `Problem ${i}`, value: "Not confirmed", caution: true })),
  notes: [],
});

function Pair({ a, b }: { a: ResultDetails; b: ResultDetails }) {
  return (
    <>
      <div className="toolbar" data-testid="bar">
        <button type="button">More</button>
      </div>
      <h2 id="top" tabIndex={-1}>
        Top
      </h2>
      <ResultBar prefix="a" statusTestId="a-status" lines={["Line A"]} details={a} onDismiss={() => undefined} fallbackId="top" />
      <ResultBar prefix="b" statusTestId="b-status" lines={["Line B"]} details={b} onDismiss={() => undefined} fallbackId="top" />
    </>
  );
}

test("the Details button is named by how many items need attention, and its dot is decorative", () => {
  render(<Pair a={caution(1)} b={caution(3)} />);
  expect(screen.getByRole("button", { name: "Details, 1 item needs attention" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Details, 3 items need attention" })).toBeInTheDocument();
  expect(screen.getByTestId("a-badge")).toHaveAttribute("aria-hidden", "true");
});

test("a clean result has a plain Details button and says nothing extra", () => {
  render(<Pair a={clean} b={clean} />);
  expect(screen.getAllByRole("button", { name: "Details" })).toHaveLength(2);
  expect(screen.queryByTestId("a-badge")).toBeNull();
  expect(screen.queryByTestId("a-attention")).toBeNull();
});

test("the live region carries hidden text after the line, so a screen reader announces what needs attention", () => {
  render(<Pair a={caution(1)} b={caution(2)} />);
  const status = screen.getByTestId("a-status");
  expect(status).toHaveAttribute("aria-live", "polite");
  const hint = screen.getByTestId("a-attention");
  expect(status).toContainElement(hint);
  expect(hint).toHaveClass("sr-only");
  expect(hint).toHaveTextContent("1 item needs attention, see Details");
  expect(screen.getByTestId("b-attention")).toHaveTextContent("2 items need attention, see Details");
  // The visible line itself has no extra wording.
  expect(status.querySelector("p")).toHaveTextContent(/^Line A$/);
});

test("opening one result's panel closes the other, and the one that closed does not take focus", async () => {
  const user = userEvent.setup();
  render(<Pair a={caution(1)} b={caution(1)} />);
  const [ta, tb] = screen.getAllByRole("button", { name: /^Details/ });
  await user.click(ta as HTMLElement);
  expect(screen.getByTestId("a-details")).toBeVisible();
  await user.click(tb as HTMLElement);
  expect(screen.getByTestId("b-details")).toBeVisible();
  expect(screen.getByTestId("a-details")).not.toBeVisible();
  expect(ta).toHaveAttribute("aria-expanded", "false");
  expect(ta).not.toHaveFocus();
  expect(screen.getByTestId("b-details").querySelector("h3")).toHaveFocus();
});

test("the panel starts below the toolbar", async () => {
  const user = userEvent.setup();
  render(<Pair a={clean} b={clean} />);
  const bar = screen.getByTestId("bar");
  bar.getBoundingClientRect = () => ({ bottom: 52 }) as DOMRect;
  await user.click(screen.getAllByRole("button", { name: "Details" })[0] as HTMLElement);
  expect(screen.getByTestId("a-details").style.top).toBe("52px");
});

test("Escape closes the panel when focus is in it or nowhere, and not when focus is in another control", async () => {
  const user = userEvent.setup();
  render(<Pair a={clean} b={clean} />);
  const toggle = screen.getAllByRole("button", { name: "Details" })[0] as HTMLElement;
  await user.click(toggle);
  // Focus is in another control (a menu, say): that control owns Escape.
  screen.getByRole("button", { name: "More" }).focus();
  await user.keyboard("{Escape}");
  expect(screen.getByTestId("a-details")).toBeVisible();
  // Focus on the page body: the panel closes, and the toggle gets focus back.
  (document.activeElement as HTMLElement).blur();
  await user.keyboard("{Escape}");
  expect(screen.getByTestId("a-details")).not.toBeVisible();
  expect(toggle).toHaveFocus();
  // Focus inside the panel.
  await user.click(toggle);
  expect(screen.getByTestId("a-details").querySelector("h3")).toHaveFocus();
  await user.keyboard("{Escape}");
  expect(screen.getByTestId("a-details")).not.toBeVisible();
  expect(toggle).toHaveFocus();
});

test("closing with the button gives focus back to the Details button", async () => {
  const user = userEvent.setup();
  render(<Pair a={clean} b={clean} />);
  const toggle = screen.getAllByRole("button", { name: "Details" })[0] as HTMLElement;
  await user.click(toggle);
  await user.click(screen.getByTestId("a-details-close"));
  expect(toggle).toHaveFocus();
});
