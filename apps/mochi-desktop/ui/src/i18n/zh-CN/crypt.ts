import type { Catalogue } from "../types";
import type { crypt as enCrypt } from "../en/crypt";
// Passphrases, encryption, the recovery record, rekeying, search coverage.
// Nothing here may say an archive is "safe", "secure", "backed up", or
// "preserved", or that a passphrase was "revoked" (spec §23.3; AGENTS.md).
export const crypt = {
  "crypt.lostPassphrase":
    "如果您丢失了此压缩文件的所有口令，其中的内容就无法再找回。没有任何恢复办法：文件本身、MOCHI 或修复功能都无法提供口令。请把口令保存在与压缩文件及其副本无关的地方。",
  "crypt.dedupLeak":
    "相同的内容只存储一次，加密的压缩文件也是如此。能够比较修改前后文件的人，可以推断出新增的内容原先是否已经在其中。",
  "crypt.hiddenWithKey": "已加密封存：文件名、属性、文件大小、文件夹结构和内容。",
  "crypt.visible1": "压缩文件 ID、提交的数量以及它们之间的父子链接。",
  "crypt.visible2": "已存储对象的数量、种类和大小，以及某次提交是否新增了数据。",
  "crypt.visible3": "密钥信封：它们的 ID，以及猜测每个口令的难度。",
  "crypt.visible4": "压缩后的大小（内容先压缩，再加密）。",
  "crypt.notSigned":
    "提交历史没有签名。能够改写整个文件的人，可以把它回退到之前的状态，或隐去某些提交。",
  "crypt.noGenericTools":
    "标准的 zstd 和 tar 工具无法解开加密的压缩文件，因此它不能同时兼容 TAR。",
  "crypt.wrongPassphrase": "该口令无法打开此压缩文件。",
  "crypt.wrongPassphraseNote": "这并不能说明压缩文件本身的状况。请检查口令后重试。",
  "crypt.removeNotRevocation":
    "移除口令并不等于让它失效。知道该口令并保留了文件副本的人，仍然可以打开那份副本，被移除的密钥信封也仍留在此文件的历史记录中。只有重新加密到新文件，才会得到新的数据密钥，而这也无法收回已经产生的副本。",
  "crypt.reencryptNote":
    "重新加密会用新的数据密钥和新口令写出一个新文件。原文件不会改变，仍可用旧口令打开；如果想让它消失，请自行处理。已经产生的副本无法收回。",
  "crypt.rekeyNeedsUnlock": "请先用口令打开压缩文件：更改口令需要密钥。",
  "crypt.recovery1":
    "恢复记录是一份额外的数据，可以重建此文件中有限范围的损坏。它不是第二份副本，带有恢复记录的压缩文件也不能防止文件本身丢失。",
  "crypt.recovery2":
    "在每组已存储的数据内，它最多可以重建大约所选百分比的损坏数据块。超出部分只会被报告，并保持原样；除此之外不作任何承诺。",
  "crypt.recovery3": "它不涵盖最新提交的页脚，也不涵盖恢复数据本身。",
  "crypt.recovery4": "文件大小会增加约您所选的百分比；提交很小时增加得更多。",
  "crypt.recovery5": "它在创建压缩文件时确定。同时请另外保留一份独立的副本。",
  "crypt.recoveryLabel": "{percent}% 额外数据",
  "crypt.rekeyAdded":
    "已提交，提交 {seq}（{status}）：已为 {file} 添加一个口令。数据密钥和数据没有改变。",
  "crypt.rekeyRemoved":
    "已提交，提交 {seq}（{status}）：已从 {file} 当前的密钥集合中移除一个口令。数据密钥和数据没有改变。",
  "crypt.rekeyReencryptedOne": "已写出 {file}：{n} 个快照，使用新的数据密钥和新口令。",
  "crypt.rekeyReencryptedMany": "已写出 {file}：{n} 个快照，使用新的数据密钥和新口令。",
  "crypt.rekeyUnknownSeq": "?",
  "crypt.rekeyDefaultStatus": "LOCAL_COMMITTED",
  "crypt.rekeyDegraded": "DEGRADED：目录项尚未确认已持久写入（{note}）。",
  "crypt.rekeyReopenError":
    "无法用手头的密钥重新打开压缩文件（{code}）：{message} 请用仍在其密钥集合中的口令重新打开。",
  "crypt.rangeNone": "无",
  "crypt.rangeSeparator": "，",
  "crypt.coverageSearchedOne":
    "已搜索所请求的 {requested} 个快照中的 {searched} 个（提交 {ranges}）；共检查了 {examined} 个项目。",
  "crypt.coverageSearchedMany":
    "已搜索所请求的 {requested} 个快照中的 {searched} 个（提交 {ranges}）；共检查了 {examined} 个项目。",
  "crypt.coverageComplete": "所请求的快照已全部搜索：没有匹配，就表示那里没有匹配的名称。",
  "crypt.coveragePartial":
    "搜索范围为 PARTIAL（不完整）。部分所请求的快照没有被搜索，因此没有找到内容并不能证明没有匹配项。",
  "crypt.coverageUnavailable": "未搜索：提交 {seq}（{code}）：{message}",
  "crypt.coverageOpenedSeparately":
    "由于最新版本的目录是根据快照清单重建的，提交 {ranges} 是逐个打开的。",
  "crypt.coverageFromManifest": "最新版本的目录映像已损坏；改用快照清单代替。",
  "crypt.coverageContentNotSearched": "只搜索了名称，没有搜索文件内容。尚不支持全文搜索。",
  // Passphrase form
  "crypt.enterPassphrase": "请输入口令。",
  "crypt.passphrasesDiffer": "两次输入的口令不一致。没有执行任何操作。",
  "crypt.notDone": "未执行",
  "crypt.repeatIt": "再输入一次",
  // Rekey screen
  "rekey.heading": "口令",
  "rekey.notEncrypted": "此压缩文件没有加密，因此没有可更改的口令。",
  "rekey.openEncrypted": "请打开一个加密的压缩文件来更改其口令。",
  "rekey.listFailed": "无法列出密钥信封",
  "rekey.envelopes": "密钥信封",
  "rekey.envelopeLine": " · 由提交 {commit} 写入 · {kdf}",
  "rekey.envelopeOpened": " · 本次已用它打开此压缩文件",
  "rekey.add": "添加口令…",
  "rekey.remove": "移除口令…",
  "rekey.removeLast": "不能移除最后一个口令",
  "rekey.reencrypt": "重新加密到新文件…",
  "rekey.addGroup": "添加口令",
  "rekey.newPassphrase": "新口令",
  "rekey.addSubmit": "添加口令",
  "rekey.removeGroup": "移除口令",
  "rekey.removeIntro":
    "请选择要移除的密钥信封。如果您不知道某个口令属于哪一个，标有“已打开此压缩文件”的就是您输入的那个。",
  "rekey.openedThis": "（已打开此压缩文件）",
  "rekey.removeThis": "移除此口令",
  "rekey.removeThisEnvelope": "移除密钥信封 {id} 的口令",
  "rekey.reencryptGroup": "重新加密",
  "rekey.reencryptHeading": "重新加密到新文件",
  "rekey.newFile": "新文件：{path}",
  "rekey.newFilePassphrase": "新文件的口令",
  "rekey.reencryptSubmit": "写出新文件",
  "rekey.working": "正在处理…",
  "rekey.cancelled": "已取消。没有更改任何内容。",
  "rekey.failedNothing": "没有更改任何内容。",
  "rekey.abortedHint": "请重新打开压缩文件，查看其中的内容。",
  "rekey.startFailed": "无法启动",
  "rekey.chooseFailed": "无法选择位置",
  // Search
  "search.didNotRun": "搜索没有运行（{code}）：{message}",
  "search.startFailed": "无法开始搜索",
  "search.matches": "匹配项",
  "search.label": "在此压缩文件中搜索名称（Ctrl+F）",
  "search.placeholder": "搜索",
  "search.clear": "清除搜索",
  "search.searching": "正在搜索…",
  "search.none": "没有匹配项",
  "search.foundOne": "{n} 个匹配项",
  "search.foundMany": "{n} 个匹配项",
  "search.firstShown": "（仅显示前 {n} 个）",
  "search.partial": " · 搜索范围 PARTIAL（不完整）",
} satisfies Catalogue<typeof enCrypt>;
