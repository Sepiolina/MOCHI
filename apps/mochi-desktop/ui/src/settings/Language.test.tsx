// The language selector: every language by its own name, applied at once with
// no reload, and remembered.
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test } from "vitest";

import { CATALOGUES, getLocale, LANGUAGE_KEY, setLocale, t, useLocale } from "../i18n";
import { Language } from "./Language";

afterEach(() => {
  setLocale("en");
  window.localStorage.clear();
});

function Probe() {
  useLocale();
  return (
    <>
      <p data-testid="probe">{t("common.cancel")}</p>
      <Language />
    </>
  );
}

test("the selector names each language in its own name", () => {
  render(<Language />);
  const options = screen.getAllByRole("option").map((o) => o.textContent);
  expect(options).toEqual(["English", "ไทย", "简体中文"]);
});

test("choosing a language re-renders the text in place and remembers the choice", async () => {
  const user = userEvent.setup();
  const cat = CATALOGUES["zh-CN"] as Record<string, string>;
  const original = cat["common.cancel"];
  cat["common.cancel"] = "ZH-CANCEL";
  try {
    render(<Probe />);
    expect(screen.getByTestId("probe")).toHaveTextContent("Cancel");
    await user.selectOptions(screen.getByTestId("language-select"), "zh-CN");
    expect(getLocale()).toBe("zh-CN");
    expect(screen.getByTestId("probe")).toHaveTextContent("ZH-CANCEL");
    expect(window.localStorage.getItem(LANGUAGE_KEY)).toBe("zh-CN");
    expect(document.documentElement.lang).toBe("zh-CN");
  } finally {
    if (original !== undefined) cat["common.cancel"] = original;
  }
});
