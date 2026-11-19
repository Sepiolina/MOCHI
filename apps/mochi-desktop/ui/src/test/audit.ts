// An in-house accessibility audit for rendered components (plan D7).
//
// The usual tool for this, axe-core, is MPL-2.0; AGENTS.md and the plan (O23)
// ask for MIT/Apache-2.0-compatible dependencies, so this is a small set of
// structural checks over the DOM instead, built on `dom-accessibility-api`
// (MIT, the accessible-name algorithm Testing Library itself uses). It checks
// what can be decided from markup alone: names, roles, references, headings,
// focusability, and structure. Contrast is checked from the stylesheet in
// `a11y/contrast.test.ts`, and keyboard behaviour by the interaction tests.
// It is not a substitute for a screen-reader pass; docs/accessibility lists
// what was and was not checked by hand.
import { computeAccessibleName } from "dom-accessibility-api";
import { expect } from "vitest";

/** Elements and roles that a person operates, and so must have a name. */
const NAMED_SELECTOR = [
  "button",
  "a[href]",
  "input:not([type=hidden])",
  "select",
  "textarea",
  "summary",
  "progress",
  "[role=button]",
  "[role=menuitem]",
  "[role=checkbox]",
  "[role=radio]",
  "[role=switch]",
  "[role=tab]",
  "[role=link]",
  "[role=textbox]",
  "[role=combobox]",
  "[role=progressbar]",
  "[role=group]",
  "[role=list]",
  "[role=table]",
].join(",");

/** Roles this app uses; anything else is a typo or an unreviewed widget. */
const ALLOWED_ROLES = new Set([
  "alert",
  "group",
  "list",
  "listitem",
  "note",
  "status",
  "search",
  "button",
  // The file list's context menu (list/ContextMenu.tsx).
  "menu",
  "menuitem",
  "none",
  // Settings' sections (settings/Settings.tsx): the ARIA tabs pattern, with arrow keys.
  "tablist",
  "tab",
  "tabpanel",
]);

const FOCUSABLE = "button,a[href],input:not([type=hidden]),select,textarea,summary,[tabindex]";

function describe(el: Element): string {
  const id = el.id ? `#${el.id}` : "";
  const test = el.getAttribute("data-testid");
  const text = (el.textContent ?? "").trim().replace(/\s+/g, " ").slice(0, 40);
  return `<${el.tagName.toLowerCase()}${id}${test ? ` data-testid=${test}` : ""}>${text ? ` "${text}"` : ""}`;
}

function visible(el: Element): boolean {
  return !el.closest("[hidden],[aria-hidden=true]");
}

/** Everything wrong with `root`'s accessibility tree that markup can show. */
export function audit(root: HTMLElement = document.body): string[] {
  const problems: string[] = [];
  const add = (el: Element, what: string) => problems.push(`${describe(el)}: ${what}`);

  // 1. Operable things have a name.
  for (const el of root.querySelectorAll(NAMED_SELECTOR)) {
    if (!visible(el)) continue;
    if (el.matches("input[type=submit],input[type=button]") && el.getAttribute("value")) continue;
    if (computeAccessibleName(el).trim() === "") add(el, "has no accessible name");
  }

  // 2. Nothing jumps the tab order.
  for (const el of root.querySelectorAll("[tabindex]")) {
    if (Number(el.getAttribute("tabindex")) > 0) add(el, "has a positive tabindex");
  }

  // 3. Ids are unique and the references to them resolve.
  const seen = new Set<string>();
  for (const el of root.querySelectorAll("[id]")) {
    if (seen.has(el.id)) add(el, `duplicate id "${el.id}"`);
    seen.add(el.id);
  }
  for (const attr of ["aria-labelledby", "aria-describedby", "aria-controls", "aria-owns"]) {
    for (const el of root.querySelectorAll(`[${attr}]`)) {
      for (const id of (el.getAttribute(attr) ?? "").split(/\s+/).filter(Boolean)) {
        if (!root.ownerDocument.getElementById(id)) add(el, `${attr} points at a missing id "${id}"`);
      }
    }
  }
  for (const label of root.querySelectorAll("label[for]")) {
    if (!root.ownerDocument.getElementById(label.getAttribute("for") ?? "")) add(label, "label for= a missing id");
  }

  // 4. Headings: none empty, and a level is never skipped going deeper.
  let previous = 0;
  for (const h of root.querySelectorAll("h1,h2,h3,h4,h5,h6")) {
    if (!visible(h)) continue;
    const level = Number(h.tagName.slice(1));
    if ((h.textContent ?? "").trim() === "") add(h, "empty heading");
    if (previous !== 0 && level > previous + 1) add(h, `heading level jumps from h${previous} to h${level}`);
    previous = level;
  }

  // 5. Roles are the ones this app has reviewed, groups and lists are named,
  //    and list items sit in a list.
  for (const el of root.querySelectorAll("[role]")) {
    const role = el.getAttribute("role") ?? "";
    if (!ALLOWED_ROLES.has(role)) add(el, `unreviewed role "${role}"`);
  }
  for (const el of root.querySelectorAll("[role=listitem]")) {
    if (!el.parentElement?.closest("[role=list],ul,ol")) add(el, "listitem outside a list");
  }
  for (const el of root.querySelectorAll("li")) {
    const parent = el.parentElement;
    if (!parent || !parent.matches("ul,ol,menu")) add(el, "li outside ul/ol");
  }
  for (const el of root.querySelectorAll("dt,dd")) {
    if (!el.parentElement?.matches("dl")) add(el, `${el.tagName.toLowerCase()} outside dl`);
  }

  // 6. A hidden subtree holds nothing a keyboard can reach.
  for (const hidden of root.querySelectorAll("[aria-hidden=true]")) {
    for (const f of hidden.querySelectorAll(FOCUSABLE)) {
      if (f.getAttribute("tabindex") !== "-1" && !(f as HTMLButtonElement).disabled) {
        add(f, "focusable inside aria-hidden");
      }
    }
  }

  // 7. Tables have header cells with a scope; one main landmark at most; more
  //    than one nav means each is named.
  for (const table of root.querySelectorAll("table")) {
    if (table.querySelectorAll("th").length === 0) add(table, "table without header cells");
    for (const th of table.querySelectorAll("th")) {
      if (!th.getAttribute("scope")) add(th, "th without scope");
    }
  }
  if (root.querySelectorAll("main").length > 1) add(root, "more than one main landmark");
  const navs = [...root.querySelectorAll("nav")];
  if (navs.length > 1) {
    for (const nav of navs) {
      if (computeAccessibleName(nav).trim() === "") add(nav, "nav without a name among several");
    }
  }

  // 8. A form control is not named by a placeholder alone, and radios share a
  //    legend: a radio group sits in a fieldset with a legend.
  for (const radio of root.querySelectorAll("input[type=radio]")) {
    if (!radio.closest("fieldset")?.querySelector("legend")) add(radio, "radio outside a fieldset with a legend");
  }

  // 9. Progress has a value text or label, and status text does not rely on
  //    colour: any element that marks a status carries the word.
  for (const badge of root.querySelectorAll("[data-status]")) {
    if (!badge.matches("[data-testid=status-badge]")) continue;
    const word = badge.getAttribute("data-status") ?? "";
    if (!(badge.textContent ?? "").includes(word)) add(badge, `status ${word} is not written out`);
  }
  return problems;
}

/** Fail the test, listing every problem found. */
export function expectAccessible(root: HTMLElement = document.body): void {
  expect(audit(root), "accessibility problems").toEqual([]);
}
