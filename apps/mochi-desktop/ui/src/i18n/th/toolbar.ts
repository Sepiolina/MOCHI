import type { Catalogue } from "../types";
import type { toolbar as enToolbar } from "../en/toolbar";
// The toolbar and its menu. Labels are one or two words; the longer
// explanation is the button's tooltip.
export const toolbar = {
  "toolbar.label": "การทำงาน",
  "toolbar.test": "ทดสอบ",
  "toolbar.testTip": "ทดสอบไฟล์บีบอัดนี้",
  "toolbar.readOnly": "ไฟล์บีบอัดรูปแบบอื่นอ่านได้อย่างเดียว",
  "toolbar.info": "รายละเอียด",
  "toolbar.infoTip": "แสดงหรือซ่อนรายละเอียด (Alt+Enter)",
  "toolbar.openFirst": "เปิดไฟล์บีบอัดก่อน",
  "toolbar.openTip": "เปิดไฟล์บีบอัด (Ctrl+O)",
  "toolbar.more": "เพิ่มเติม",
  "toolbar.repair": "ซ่อม",
  "toolbar.foreign": "รูปแบบอื่น…",
  "toolbar.helpTip": "ความช่วยเหลือและปุ่มลัด (F1)",
  "toolbar.find": "ค้นหา",
  "toolbar.findTip": "ค้นหาชื่อภายในไฟล์บีบอัดหลายไฟล์ (Ctrl+Shift+F)",
  // The window's own title bar (release 0.9.10) and its buttons.
  "window.controls": "หน้าต่าง",
  "window.minimize": "ย่อหน้าต่าง",
  "window.maximize": "ขยายเต็มจอ",
  "window.restore": "คืนขนาดเดิม",
  "window.close": "ปิด",
  // Why a button or menu item is off (shell/availability.ts); the item stays visible.
  "avail.noArchive": "เปิดไฟล์บีบอัดก่อน",
  "avail.locked": "กรอกวลีรหัสผ่านของไฟล์บีบอัดก่อน",
  "avail.noSelection": "เลือกรายการก่อน",
  "avail.emptyList": "ไม่มีรายการให้เลือก",
  "avail.busy": "รอให้งานปัจจุบันเสร็จก่อน",
  "avail.notEncrypted": "เฉพาะไฟล์บีบอัด MOCHI ที่เข้ารหัสเท่านั้นที่มีวลีรหัสผ่าน",
} satisfies Catalogue<typeof enToolbar>;
