import { render } from "@testing-library/react";
import { createElement } from "react";
import { expect, test } from "vitest";

import { FileTypeIcon } from "../icons";
import { extensionOf, iconKindOf, typeLabelOf } from "./fileKind";

test("a known extension picks its icon, a folder is a folder, a link is a link", () => {
  expect(iconKindOf("directory", "photos.png")).toBe("folder");
  expect(iconKindOf("file", "a.TIF")).toBe("image");
  expect(iconKindOf("file", "species.csv")).toBe("sheet");
  expect(iconKindOf("file", "notes.pdf")).toBe("pdf");
  expect(iconKindOf("file", "readme.txt")).toBe("text");
  expect(iconKindOf("file", "song.MP3")).toBe("audio");
  expect(iconKindOf("file", "track.flac")).toBe("audio");
  expect(iconKindOf("file", "clip.mkv")).toBe("video");
  expect(iconKindOf("file", "photos.zip")).toBe("archive");
  expect(iconKindOf("file", "main.rs")).toBe("code");
  expect(iconKindOf("file", "Makefile")).toBe("generic");
  expect(iconKindOf("link", "x.png")).toBe("link");
  expect(iconKindOf("special", "x.png")).toBe("generic");
});

test("a hostile extension maps to the generic icon and is never shown unless it is plain", () => {
  for (const name of [
    "a.constructor",
    "a.__proto__",
    "a.toString",
    '<img src=x onerror="alert(1)">',
    "a.<img src=x>",
    'x.png"onload="alert(1)',
    "a.pn g",
    "a.",
    ".png",
    "a.‮png",
  ]) {
    expect(iconKindOf("file", name), name).toBe("generic");
    expect(extensionOf(name), name).toBeNull();
    expect(typeLabelOf("file", name), name).toBe("File");
  }
});

test("only a short ASCII extension reaches the Type column, in capitals", () => {
  expect(typeLabelOf("file", "lake.tif")).toBe("TIF");
  expect(typeLabelOf("file", "a.longerext")).toBe("File");
  expect(typeLabelOf("directory", "a.png")).toBe("Folder");
});

test("every icon kind renders one decorative svg with no text from outside", () => {
  for (const kind of [
    "folder",
    "image",
    "audio",
    "video",
    "archive",
    "code",
    "sheet",
    "pdf",
    "text",
    "generic",
    "link",
  ] as const) {
    const { container, unmount } = render(createElement(FileTypeIcon, { kind }));
    const svgs = container.querySelectorAll("svg");
    expect(svgs).toHaveLength(1);
    expect(svgs[0]?.getAttribute("aria-hidden")).toBe("true");
    expect(svgs[0]?.getAttribute("focusable")).toBe("false");
    expect(container.textContent).toBe("");
    unmount();
  }
});
