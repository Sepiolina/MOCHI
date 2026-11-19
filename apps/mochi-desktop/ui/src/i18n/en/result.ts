// The compact result of a create, add, remove, or extract: one short line, and
// a Details panel for everything else (owner request, release 0.9.3). Nothing
// here is a verification, so nothing gets success styling (spec §23.3).
export const result = {
  "result.details": "Details",
  "result.detailsAttention": "Details, {items}",
  "result.attentionOne": "{n} item needs attention",
  "result.attentionMany": "{n} items need attention",
  "result.attentionHint": "{items}, see Details",
  "result.dismiss": "Dismiss result",
} as const;
