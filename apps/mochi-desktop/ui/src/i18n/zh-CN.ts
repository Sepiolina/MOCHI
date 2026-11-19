// The zh-CN message catalogue, assembled from one file per screen (mirrors en.ts).
// A key missing here, or not in English, fails `pnpm typecheck`.
import type { Catalogue } from "./types";
import type { en } from "./en";
import { app } from "./zh-CN/app";
import { browser } from "./zh-CN/browser";
import { check } from "./zh-CN/check";
import { common } from "./zh-CN/common";
import { crypt } from "./zh-CN/crypt";
import { extract } from "./zh-CN/extract";
import { find } from "./zh-CN/find";
import { foreign } from "./zh-CN/foreign";
import { help } from "./zh-CN/help";
import { info } from "./zh-CN/info";
import { quick } from "./zh-CN/quick";
import { result } from "./zh-CN/result";
import { scan } from "./zh-CN/scan";
import { settings } from "./zh-CN/settings";
import { start } from "./zh-CN/start";
import { toolbar } from "./zh-CN/toolbar";
import { transfer } from "./zh-CN/transfer";
import { update } from "./zh-CN/update";
import { write } from "./zh-CN/write";

export const zhCN = {
  ...app,
  ...browser,
  ...check,
  ...common,
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
} satisfies Catalogue<typeof en>;
