import { expect, test } from "vitest";

import {
  ALL_PRESETS,
  extensionOf,
  fitPreset,
  FORMATS,
  hasLevel,
  isMochi,
  nameForFormat,
  presetsFor,
  stripArchiveExtension,
} from "./format";

test("the five formats, .mochi first, each with its own extension", () => {
  expect(FORMATS).toEqual(["mochi", "zip", "tar", "tar_gz", "tar_zst"]);
  expect(FORMATS.map(extensionOf)).toEqual([".mochi", ".zip", ".tar", ".tar.gz", ".tar.zst"]);
});

test("a name takes the extension of the chosen format, as Rust does", () => {
  expect(nameForFormat("Photos", "zip")).toBe("Photos.zip");
  expect(nameForFormat("Photos.mochi", "zip")).toBe("Photos.zip");
  expect(nameForFormat("Photos.ZIP", "mochi")).toBe("Photos.mochi");
  expect(nameForFormat("Photos.tar.zst", "tar_gz")).toBe("Photos.tar.gz");
  expect(nameForFormat("Photos.tar.gz", "tar")).toBe("Photos.tar");
  expect(nameForFormat("a.tar.GZ", "tar_gz")).toBe("a.tar.GZ");
  // A dot that is not an archive extension is part of the name.
  expect(nameForFormat("report v2.1", "zip")).toBe("report v2.1.zip");
  expect(nameForFormat(".zip", "zip")).toBe(".zip.zip");
  expect(nameForFormat("Grüße ☃", "tar_zst")).toBe("Grüße ☃.tar.zst");
  expect(stripArchiveExtension("x.tgz")).toBe("x");
});

test("only plain tar has no level, and only .mochi has the MOCHI options", () => {
  expect(FORMATS.filter((f) => !hasLevel(f))).toEqual(["tar"]);
  expect(FORMATS.filter(isMochi)).toEqual(["mochi"]);
});

test("ZIP and tar.gz offer three presets; .mochi and tar.zst all seven; plain tar none", () => {
  expect(presetsFor("zip")).toEqual(["fast", "balanced", "maximum"]);
  expect(presetsFor("tar_gz")).toEqual(["fast", "balanced", "maximum"]);
  expect(presetsFor("mochi")).toEqual(ALL_PRESETS);
  expect(presetsFor("tar_zst")).toEqual(ALL_PRESETS);
  expect(presetsFor("tar")).toEqual([]);
});

test("a preset the format lacks becomes the nearest one it offers", () => {
  expect(ALL_PRESETS.map((p) => fitPreset("zip", p))).toEqual([
    "fast",
    "fast",
    "fast",
    "balanced",
    "maximum",
    "maximum",
    "maximum",
  ]);
  for (const p of ALL_PRESETS) {
    expect(fitPreset("tar_zst", p)).toBe(p);
    expect(fitPreset("mochi", p)).toBe(p);
    expect(fitPreset("tar", p)).toBe(p);
  }
});
