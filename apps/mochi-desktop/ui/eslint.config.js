// ESLint flat config. AGENTS.md "Tauri v2 rules": archive-supplied strings
// render as text only. `eslint-plugin-react`'s `react/no-danger` stops at
// ESLint 9 (out of support), so the maintained `@eslint-react` rules carry
// it, with a core-ESLint backstop that bans the attribute by name, and
// `ci/check-invariants.sh` greps for it as well.
import js from "@eslint/js";
import eslintReact from "@eslint-react/eslint-plugin";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";
import tseslint from "typescript-eslint";

// The same ban as `no-restricted-syntax` below, plus (for screens, not tests)
// user-visible English in JSX: text and the attributes a screen reader reads
// come from the message catalogue (src/i18n), so a string cannot dodge the
// wording tests (§23.3) or the typed lookup by being written inline.
const HTML_BANS = [
  {
    selector: "JSXAttribute[name.name='dangerouslySetInnerHTML']",
    message: "Render archive strings as text only (AGENTS.md, spec §23.3 #9).",
  },
  {
    selector: "AssignmentExpression[left.property.name=/^(innerHTML|outerHTML)$/]",
    message: "Never build HTML from strings (AGENTS.md).",
  },
  {
    selector: "CallExpression[callee.property.name='insertAdjacentHTML']",
    message: "Never build HTML from strings (AGENTS.md).",
  },
];

const INLINE_ENGLISH = [
  {
    selector: "JSXText[value=/[A-Za-z]{3,}/]",
    message: "User-visible text belongs in the message catalogue: use t() (src/i18n).",
  },
  {
    selector:
      "JSXAttribute[name.name=/^(aria-label|aria-description|aria-valuetext|aria-placeholder|title|placeholder|alt)$/] > Literal[value=/[A-Za-z]{3,}/]",
    message: "User-visible text belongs in the message catalogue: use t() (src/i18n).",
  },
];

export default tseslint.config(
  { ignores: ["dist", "node_modules", "src/ipc/generated"] },
  {
    files: ["**/*.{ts,tsx}"],
    extends: [
      js.configs.recommended,
      ...tseslint.configs.strict,
      eslintReact.configs["recommended-typescript"],
    ],
    languageOptions: {
      ecmaVersion: 2023,
      globals: globals.browser,
    },
    plugins: { "react-hooks": reactHooks },
    rules: {
      ...reactHooks.configs.recommended.rules,
      "@eslint-react/dom-no-dangerously-set-innerhtml": "error",
      "@eslint-react/dom-no-dangerously-set-innerhtml-with-children": "error",
      "@eslint-react/dom-no-script-url": "error",
      "no-restricted-syntax": ["error", ...HTML_BANS],
      "no-eval": "error",
      "no-implied-eval": "error",
      "no-new-func": "error",
    },
  },
  {
    files: ["src/**/*.tsx"],
    ignores: ["src/**/*.test.tsx"],
    rules: { "no-restricted-syntax": ["error", ...HTML_BANS, ...INLINE_ENGLISH] },
  },
);
