import type { Catalogue } from "../types";
import type { toolbar as enToolbar } from "../en/toolbar";
// The toolbar and its menu. Labels are one or two words; the longer
// explanation is the button's tooltip.
export const toolbar = {
  "toolbar.label": "操作",
  "toolbar.test": "测试",
  "toolbar.testTip": "测试此压缩文件",
  "toolbar.readOnly": "其他格式的压缩文件为只读",
  "toolbar.info": "详细信息",
  "toolbar.infoTip": "显示或隐藏详细信息（Alt+Enter）",
  "toolbar.openFirst": "请先打开压缩文件",
  "toolbar.openTip": "打开压缩文件（Ctrl+O）",
  "toolbar.more": "更多",
  "toolbar.repair": "修复",
  "toolbar.foreign": "其他格式…",
  "toolbar.helpTip": "帮助和键盘快捷键（F1）",
  "toolbar.find": "查找",
  "toolbar.findTip": "在多个压缩文件中查找名称（Ctrl+Shift+F）",
  // The window's own title bar (release 0.9.10) and its buttons.
  "window.controls": "窗口",
  "window.minimize": "最小化",
  "window.maximize": "最大化",
  "window.restore": "还原",
  "window.close": "关闭",
  // Why a button or menu item is off (shell/availability.ts); the item stays visible.
  "avail.noArchive": "请先打开压缩文件",
  "avail.locked": "请先输入压缩文件的口令",
  "avail.noSelection": "请先选择一个项目",
  "avail.emptyList": "这里没有可选择的项目",
  "avail.busy": "请等待当前任务完成",
  "avail.notEncrypted": "只有加密的 MOCHI 压缩文件才有口令",
} satisfies Catalogue<typeof enToolbar>;
