import type { Catalogue } from "../types";
import type { start as enStart } from "../en/start";
// The start screen and the plain wording of the main path (D10). Technical
// terms (snapshot, commit, hash) stay in the "Technical details" areas and in
// help text; the main path says "version", "files", and "folder".
export const start = {
  "start.heading": "您想做什么？",
  "start.create": "创建压缩文件",
  "start.open": "打开压缩文件",
  "start.dropZone": "将文件或压缩文件拖到此处",
  "start.settings": "设置",
  "start.dropHint": "文件和文件夹会放入新的压缩文件；压缩文件则会被打开。",
  "start.note":
    "压缩文件把多个文件打包成一个文件，它不是这些文件的第二份副本：无法失去的内容，请另外保留独立的副本。",
  "start.recent": "最近的压缩文件",
  "start.recentOpen": "打开 {name}",
  "start.recentFolder": "位于 {folder}",
  "start.forgetRecent": "清除此列表",
  "start.openRecentFailed": "无法打开该压缩文件",
  "start.openDroppedFailed": "无法打开拖入的压缩文件",
  "start.extractOpen": "打开并解压…",
  "start.showInFolder": "在文件夹中显示",
  "start.showInFolderFailed": "无法显示该文件夹",
  "start.technical": "技术详情",
} satisfies Catalogue<typeof enStart>;
