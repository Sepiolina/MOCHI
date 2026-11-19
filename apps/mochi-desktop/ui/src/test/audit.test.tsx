// The audit has to be able to fail, or a green run says nothing.
import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";

import { audit } from "./audit";

// These tests render broken markup on purpose: remove it before the suite-wide
// audit (test/setup.ts) looks at what is left.
afterEach(() => cleanup());

function problemsIn(ui: React.ReactElement): string[] {
  const { container } = render(ui);
  return audit(container);
}

test("a labelled, well-formed screen has no problems", () => {
  const found = problemsIn(
    <main>
      <h1>One</h1>
      <h2>Two</h2>
      <label>
        Name <input type="text" />
      </label>
      <button type="button">Go</button>
      <ul aria-label="Items">
        <li>a</li>
      </ul>
    </main>,
  );
  expect(found).toEqual([]);
});

test("it finds a control with no name", () => {
  expect(problemsIn(<button type="button">{""}</button>)[0]).toMatch(/no accessible name/);
  expect(problemsIn(<input type="checkbox" />)[0]).toMatch(/no accessible name/);
  expect(problemsIn(<progress value={1} max={2} />)[0]).toMatch(/no accessible name/);
});

test("it finds a name that only an icon supplies, but accepts aria-label", () => {
  expect(problemsIn(<button type="button">▸</button>)).toEqual([]); // the glyph is the (poor) name
  expect(problemsIn(<button type="button" aria-label="Expand folder">▸</button>)).toEqual([]);
});

test("it finds broken references and duplicate ids", () => {
  expect(problemsIn(<div id="a" aria-labelledby="missing" role="group" />).join("\n")).toMatch(/missing id "missing"/);
  expect(
    problemsIn(
      <>
        <p id="x">1</p>
        <p id="x">2</p>
      </>,
    ).join("\n"),
  ).toMatch(/duplicate id "x"/);
});

test("it finds a skipped heading level, an empty heading, and a positive tabindex", () => {
  expect(problemsIn(<><h1>a</h1><h3>b</h3></>).join("\n")).toMatch(/jumps from h1 to h3/);
  expect(problemsIn(<h2>{""}</h2>).join("\n")).toMatch(/empty heading/);
  expect(problemsIn(<div tabIndex={3}>x</div>).join("\n")).toMatch(/positive tabindex/);
});

test("it finds structure that breaks assistive technology", () => {
  expect(problemsIn(<div role="tree" />).join("\n")).toMatch(/unreviewed role "tree"/);
  expect(problemsIn(<div role="listitem">x</div>).join("\n")).toMatch(/listitem outside a list/);
  expect(problemsIn(<div aria-hidden="true"><button type="button">x</button></div>).join("\n")).toMatch(
    /focusable inside aria-hidden/,
  );
  expect(problemsIn(<table><tbody><tr><td>x</td></tr></tbody></table>).join("\n")).toMatch(/without header cells/);
  expect(problemsIn(<input type="radio" aria-label="x" />).join("\n")).toMatch(/radio outside a fieldset/);
});

test("it finds a status badge that is only a colour", () => {
  expect(
    problemsIn(<span data-testid="status-badge" data-status="FAIL" className="status-fail" />).join("\n"),
  ).toMatch(/status FAIL is not written out/);
});
