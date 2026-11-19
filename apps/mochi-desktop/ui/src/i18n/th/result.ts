import type { Catalogue } from "../types";
import type { result as enResult } from "../en/result";
// The compact result of a create, add, remove, or extract: one short line, and
// a Details panel for everything else (owner request, release 0.9.3). Nothing
// here is a verification, so nothing gets success styling (spec §23.3).
export const result = {
  "result.details": "รายละเอียด",
  "result.detailsAttention": "รายละเอียด, {items}",
  "result.attentionOne": "{n} รายการต้องตรวจดู",
  "result.attentionMany": "{n} รายการต้องตรวจดู",
  "result.attentionHint": "{items} ดูรายละเอียด",
  "result.dismiss": "ปิดผลลัพธ์",
} satisfies Catalogue<typeof enResult>;
