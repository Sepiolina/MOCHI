import type { Catalogue } from "../types";
import type { app as enApp } from "../en/app";
// The window itself: title and section navigation.
export const app = {
  "app.title": "MOCHI",
  "app.sectionPassphrases": "口令",
  "app.sectionCheck": "测试、状况和修复",
  "app.sectionSettings": "设置",
} satisfies Catalogue<typeof enApp>;
