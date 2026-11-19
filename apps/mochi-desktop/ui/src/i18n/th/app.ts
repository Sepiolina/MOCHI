import type { Catalogue } from "../types";
import type { app as enApp } from "../en/app";
// The window itself: title and section navigation.
export const app = {
  "app.title": "MOCHI",
  "app.sectionPassphrases": "วลีรหัสผ่าน",
  "app.sectionCheck": "ทดสอบ สภาพ และซ่อม",
  "app.sectionSettings": "การตั้งค่า",
} satisfies Catalogue<typeof enApp>;
