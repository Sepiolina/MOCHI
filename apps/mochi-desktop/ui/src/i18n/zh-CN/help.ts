import type { Catalogue } from "../types";
import type { help as enHelp } from "../en/help";
// Help, User guide (audit F8). The guide is the command-line guide from docs/user, bundled
// with the installer and opened offline; the labels say so before it is opened.
export const help = {
  "help.heading": "帮助",
  "help.toolbar": "帮助",
  "help.keys":
    "在列表中，方向键在各项之间移动，Page Up、Page Down、Home 和 End 可快速跳转，空格键选中，Enter 打开文件夹。",
  "help.shortcuts":
    "Ctrl+O 打开压缩文件，Ctrl+F 搜索其中的名称，Ctrl+A 选中当前文件夹中的全部项目，Esc 取消选择，Backspace 返回上一级，Alt+向左键和 Alt+向右键在已打开的文件夹之间后退和前进，Delete 删除所选项目。按住 Ctrl 或 Shift 单击可选择多项。把项目从列表拖到文件夹中即可解压；把文件拖到窗口上即可添加。Ctrl+Shift+F 可在你选择的文件夹下的多个压缩文件中查找名称。Ctrl+N 新建压缩文件，Ctrl+E 解压，Alt+Enter 显示或隐藏详细信息，F1 打开本帮助；这些操作和其他操作也都在“更多”菜单中。",
  "help.userGuide": "用户指南（命令行）",
  "help.userGuideNote":
    "此指南针对 mochi 命令行编写，不是针对本应用；开头会指出对应的界面。将用默认浏览器打开随 MOCHI 一起安装的副本，无需联网。",
  "help.requested": "已将用户指南交给默认的网页浏览程序打开。",
  "help.failed": "无法打开用户指南（{code}）：{message}",
  "help.failedPlain": "无法打开用户指南。",
} satisfies Catalogue<typeof enHelp>;
