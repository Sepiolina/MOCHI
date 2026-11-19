import type { Catalogue } from "../types";
import type { result as enResult } from "../en/result";
// The compact result of a create, add, remove, or extract: one short line, and
// a Details panel for everything else (owner request, release 0.9.3). Nothing
// here is a verification, so nothing gets success styling (spec §23.3).
export const result = {
  "result.details": "详细信息",
  "result.detailsAttention": "详细信息，{items}",
  "result.attentionOne": "{n} 项需要注意",
  "result.attentionMany": "{n} 项需要注意",
  "result.attentionHint": "{items}，请查看详细信息",
  "result.dismiss": "关闭结果",
} satisfies Catalogue<typeof enResult>;
