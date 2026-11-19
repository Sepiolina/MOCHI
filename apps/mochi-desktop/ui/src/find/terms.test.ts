// Cutting a pasted list into names (spec Annex B.2.21): the separators, quotes, repeats, and
// the bounds Rust applies again.
import { expect, test } from "vitest";

import { lastComponent, looksLikeList, MAX_TERM_BYTES, MAX_TERMS, parseTerms } from "./terms";
import type { ParseOptions } from "./terms";

const auto: ParseOptions = { separator: "auto", ignoreCase: true, namesOnly: false };

test("auto splits on line breaks, commas, semicolons and tabs, never on spaces", () => {
  const r = parseTerms("a.pdf\r\nb.txt, c d.doc;e.jpg\tf.png\n\n  ,  ", auto);
  expect(r.terms).toEqual(["a.pdf", "b.txt", "c d.doc", "e.jpg", "f.png"]);
  expect(r).toMatchObject({ duplicates: 0, tooLong: 0, overLimit: 0 });
});

test("each separator can be chosen alone", () => {
  const text = "a,b;c\td e\nf";
  expect(parseTerms(text, { ...auto, separator: "lines" }).terms).toEqual(["a,b;c\td e", "f"]);
  expect(parseTerms(text, { ...auto, separator: "commas" }).terms).toEqual(["a", "b;c\td e\nf"]);
  expect(parseTerms(text, { ...auto, separator: "semicolons" }).terms).toEqual(["a,b", "c\td e\nf"]);
  expect(parseTerms(text, { ...auto, separator: "tabs" }).terms).toEqual(["a,b;c", "d e\nf"]);
  expect(parseTerms(text, { ...auto, separator: "spaces" }).terms).toEqual(["a,b;c", "d", "e", "f"]);
});

test("quotes are dropped, repeats are counted once, and case decides what repeats", () => {
  const r = parseTerms('"C:\\Work\\Report.pdf"\nreport.PDF\n\'x.txt\'\nx.txt', auto);
  expect(r.terms).toEqual(["C:\\Work\\Report.pdf", "report.PDF", "x.txt"]);
  expect(r.duplicates).toBe(1);
  const cased = parseTerms("A.txt\na.txt\nA.txt", { ...auto, ignoreCase: false });
  expect(cased.terms).toEqual(["A.txt", "a.txt"]);
  expect(cased.duplicates).toBe(1);
});

test("names only keeps the last path component, by either separator", () => {
  const r = parseTerms("C:\\Work\\Report.pdf\n/home/me/report.pdf\ndocs/a.txt", { ...auto, namesOnly: true });
  expect(r.terms).toEqual(["Report.pdf", "a.txt"]);
  expect(r.duplicates).toBe(1);
  expect(lastComponent("a/b\\c")).toBe("c");
  expect(lastComponent("plain")).toBe("plain");
});

test("names over the byte limit and past the count limit are left out and counted", () => {
  const long = "é".repeat(MAX_TERM_BYTES / 2 + 1);
  const many = Array.from({ length: MAX_TERMS + 5 }, (_, i) => `f${i}`).join("\n");
  const r = parseTerms(`${long}\n${many}`, auto);
  expect(r.terms).toHaveLength(MAX_TERMS);
  expect(r.tooLong).toBe(1);
  expect(r.overLimit).toBe(5);
});

test("a list is told apart from one name", () => {
  expect(looksLikeList("a.pdf\nb.pdf")).toBe(true);
  expect(looksLikeList("a.pdf, b.pdf")).toBe(true);
  expect(looksLikeList("my report.pdf")).toBe(false);
  expect(looksLikeList("a.pdf\n")).toBe(false);
});
