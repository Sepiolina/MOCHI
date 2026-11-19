// A translation of one English catalogue file: exactly the same keys, string
// values. `satisfies Catalogue<typeof enScreen>` makes a missing key and an
// extra key both fail `pnpm typecheck`. Placeholder parity is checked by
// i18n.test.ts.
export type Catalogue<T> = { readonly [K in keyof T]: string };
