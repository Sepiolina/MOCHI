import type { Catalogue } from "../types";
import type { start as enStart } from "../en/start";
// The start screen and the plain wording of the main path (D10). Technical
// terms (snapshot, commit, hash) stay in the "Technical details" areas and in
// help text; the main path says "version", "files", and "folder".
export const start = {
  "start.heading": "ต้องการทำอะไร",
  "start.create": "สร้างไฟล์บีบอัด",
  "start.open": "เปิดไฟล์บีบอัด",
  "start.dropZone": "วางไฟล์หรือไฟล์บีบอัดที่นี่",
  "start.settings": "การตั้งค่า",
  "start.dropHint": "ไฟล์และโฟลเดอร์จะถูกใส่ในไฟล์บีบอัดใหม่ ส่วนไฟล์บีบอัดจะถูกเปิด",
  "start.note":
    "ไฟล์บีบอัดรวมไฟล์ไว้ในไฟล์เดียว ไม่ใช่สำเนาที่สองของไฟล์เหล่านั้น ควรเก็บสำเนาแยกไว้อีกชุดสำหรับสิ่งที่ห้ามสูญหาย",
  "start.recent": "ไฟล์บีบอัดล่าสุด",
  "start.recentOpen": "เปิด {name}",
  "start.recentFolder": "ใน {folder}",
  "start.forgetRecent": "ล้างรายการนี้",
  "start.openRecentFailed": "เปิดไฟล์บีบอัดนั้นไม่สำเร็จ",
  "start.openDroppedFailed": "เปิดไฟล์บีบอัดที่วางไม่สำเร็จ",
  "start.extractOpen": "เปิดและแตกไฟล์…",
  "start.showInFolder": "แสดงในโฟลเดอร์",
  "start.showInFolderFailed": "แสดงโฟลเดอร์ไม่สำเร็จ",
  "start.technical": "รายละเอียดทางเทคนิค",
} satisfies Catalogue<typeof enStart>;
