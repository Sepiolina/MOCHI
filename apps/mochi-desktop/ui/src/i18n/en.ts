// The English message catalogue, assembled from one file per screen
// (apps/mochi-desktop/README.md "Messages"). Keys are `<screen>.<name>`;
// a key must exist here for `t()` to accept it.
import { browser } from "./en/browser";
import { app } from "./en/app";
import { check } from "./en/check";
import { common } from "./en/common";
import { crypt } from "./en/crypt";
import { extract } from "./en/extract";
import { find } from "./en/find";
import { foreign } from "./en/foreign";
import { help } from "./en/help";
import { info } from "./en/info";
import { quick } from "./en/quick";
import { result } from "./en/result";
import { scan } from "./en/scan";
import { settings } from "./en/settings";
import { start } from "./en/start";
import { toolbar } from "./en/toolbar";
import { transfer } from "./en/transfer";
import { update } from "./en/update";
import { write } from "./en/write";

export const en = {
  ...app,
  ...common,
  ...check,
  ...browser,
  ...crypt,
  ...extract,
  ...find,
  ...foreign,
  ...help,
  ...info,
  ...quick,
  ...result,
  ...scan,
  ...settings,
  ...start,
  ...toolbar,
  ...transfer,
  ...update,
  ...write,
} as const;
