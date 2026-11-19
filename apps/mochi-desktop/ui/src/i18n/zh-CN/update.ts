import type { Catalogue } from "../types";
import type { update as enUpdate } from "../en/update";
// Updates (plan D7, T13). A failure is never styled as success; an unsigned
// installer is never called verified, trusted, safe, or secure; the update's own
// signature is what authenticates an update. Release notes and the detail of a
// failure come from the network: they are parameters or text nodes, never markup.
export const update = {
  "update.heading": "更新",
  "update.intro":
    "除非您主动操作，MOCHI 不会检查更新，也不会下载或安装更新。检查更新只向 GitHub（github.com/immerh8/MOCHI/releases）发出一次请求，请求中不含文件名或压缩文件内容。",
  "update.check": "检查更新",
  "update.checking": "正在检查更新…",
  "update.startup": "MOCHI 启动时检查更新（默认关闭，需手动打开）",
  "update.startupOn": "MOCHI 每次启动时会检查一次。下载任何内容之前仍会先询问您。",
  "update.startupOff": "MOCHI 不会自行检查更新。",
  "update.signing":
    "MOCHI 的安装程序没有使用 Windows 发布者证书进行代码签名，因此运行时 Windows SmartScreen 会显示“未知发布者”：请选择“更多信息”，然后选择“仍要运行”。更新由 MOCHI 自己的更新签名验证，本应用在安装任何内容之前都会检查该签名；签名不匹配的更新会被拒绝。",
  "update.loadFailed": "无法读取更新设置。",
  "update.saveFailed": "无法保存更新设置。",
  "update.version": "您正在运行的版本是 {current}。",
  "update.upToDate": "没有找到更新的版本。您正在运行的版本是 {current}。",
  "update.available": "有新版本 {version} 可用。您正在运行的版本是 {current}。",
  "update.dialogLabel": "有新版本可用",
  "update.later": "暂不更新",
  "update.install": "下载并安装",
  "update.cancel": "取消下载",
  "update.adminPrompt": "Ubuntu 会要求您输入密码以安装软件包。",
  "update.closesApp": "在 Windows 上，安装程序启动时 MOCHI 会关闭。",
  "update.manual":
    "这份 MOCHI 不是通过 MOCHI 安装包安装的，因此无法自行更新。请从以下地址下载新版本：",
  "update.notesHeading": "发行说明",
  "update.notesNone": "此版本没有发行说明。",
  "update.notesCut": "说明在此处被截断；其余内容请见发行页面。",
  "update.installingBytes": "已下载 {size}",
  "update.installingOf": "已下载 {size}，共 {total}",
  "update.cancelled": "已取消。更新没有安装。",
  "update.installedRestart": "版本 {version} 已安装。请关闭 MOCHI 后重新打开以使用新版本。",
  "update.installFailed": "更新没有安装：{message}",
  "update.failOffline": "无法连接更新服务器。请检查网络连接后重试。没有任何更改。",
  "update.failServer": "更新服务器没有给出可用的回复。没有任何更改。（{detail}）",
  "update.failSignature": "更新的签名不匹配，已被拒绝。没有安装任何内容。",
  "update.failInstall": "更新已下载，但没有安装。（{detail}）",
  "update.failUnavailable": "此版本不支持更新。（{detail}）",
  "update.failOther": "检查更新未能完成。（{detail}）",
  "update.couldNotStart": "无法启动",
  "update.unknownError": "未知错误",
} satisfies Catalogue<typeof enUpdate>;
