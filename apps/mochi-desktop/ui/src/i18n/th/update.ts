import type { Catalogue } from "../types";
import type { update as enUpdate } from "../en/update";
// Updates (plan D7, T13). A failure is never styled as success; an unsigned
// installer is never called verified, trusted, safe, or secure; the update's own
// signature is what authenticates an update. Release notes and the detail of a
// failure come from the network: they are parameters or text nodes, never markup.
export const update = {
  "update.heading": "การอัปเดต",
  "update.intro":
    "MOCHI จะไม่ตรวจหาการอัปเดต ไม่ดาวน์โหลด และไม่ติดตั้ง จนกว่าคุณจะสั่ง การตรวจเป็นคำขอเดียวไปยัง GitHub (github.com/immerh8/MOCHI/releases) ซึ่งไม่ส่งชื่อไฟล์หรือเนื้อหาไฟล์บีบอัด",
  "update.check": "ตรวจหาการอัปเดต",
  "update.checking": "กำลังตรวจหาการอัปเดต…",
  "update.startup": "ตรวจหาการอัปเดตเมื่อ MOCHI เริ่มทำงาน (ปิดไว้จนกว่าคุณจะเปิด)",
  "update.startupOn": "MOCHI จะตรวจหนึ่งครั้งทุกครั้งที่เริ่มทำงาน และจะถามก่อนดาวน์โหลดอะไรทั้งสิ้น",
  "update.startupOff": "MOCHI จะไม่ตรวจหาการอัปเดตเอง",
  "update.signing":
    "ตัวติดตั้งของ MOCHI ไม่ได้ลงลายเซ็นโค้ดด้วยใบรับรองผู้เผยแพร่ของ Windows ดังนั้น Windows SmartScreen จะแสดง \"ผู้เผยแพร่ที่ไม่รู้จัก\" เมื่อคุณเรียกใช้: เลือก ข้อมูลเพิ่มเติม แล้วเลือก เรียกใช้ต่อไป การอัปเดตจะยืนยันด้วยลายเซ็นอัปเดตของ MOCHI เอง ซึ่งแอปนี้ตรวจก่อนติดตั้งทุกครั้ง การอัปเดตที่ลายเซ็นไม่ตรงจะถูกปฏิเสธ",
  "update.loadFailed": "อ่านการตั้งค่าการอัปเดตไม่สำเร็จ",
  "update.saveFailed": "บันทึกการตั้งค่าการอัปเดตไม่สำเร็จ",
  "update.version": "คุณกำลังใช้เวอร์ชัน {current}",
  "update.upToDate": "ไม่พบเวอร์ชันที่ใหม่กว่า คุณกำลังใช้เวอร์ชัน {current}",
  "update.available": "มีเวอร์ชัน {version} คุณกำลังใช้เวอร์ชัน {current}",
  "update.dialogLabel": "มีเวอร์ชันใหม่",
  "update.later": "ไว้ทีหลัง",
  "update.install": "ดาวน์โหลดและติดตั้ง",
  "update.cancel": "ยกเลิกการดาวน์โหลด",
  "update.adminPrompt": "Ubuntu จะขอรหัสผ่านของคุณเพื่อติดตั้งแพ็กเกจ",
  "update.closesApp": "บน Windows MOCHI จะปิดเมื่อตัวติดตั้งเริ่มทำงาน",
  "update.manual":
    "MOCHI ชุดนี้ไม่ได้ติดตั้งจากแพ็กเกจของ MOCHI จึงอัปเดตตัวเองไม่ได้ ดาวน์โหลดเวอร์ชันใหม่ได้จาก:",
  "update.notesHeading": "บันทึกประจำรุ่น",
  "update.notesNone": "รุ่นนี้ไม่มีบันทึก",
  "update.notesCut": "บันทึกถูกตัดตรงนี้ หน้ารุ่นมีส่วนที่เหลือ",
  "update.installingBytes": "ดาวน์โหลดแล้ว {size}",
  "update.installingOf": "ดาวน์โหลดแล้ว {size} จาก {total}",
  "update.cancelled": "ยกเลิกแล้ว ไม่ได้ติดตั้งการอัปเดต",
  "update.installedRestart": "ติดตั้งเวอร์ชัน {version} แล้ว ปิด MOCHI แล้วเปิดใหม่เพื่อใช้งาน",
  "update.installFailed": "ไม่ได้ติดตั้งการอัปเดต: {message}",
  "update.failOffline":
    "ติดต่อเซิร์ฟเวอร์อัปเดตไม่ได้ ตรวจสอบการเชื่อมต่อแล้วลองอีกครั้ง ไม่มีอะไรเปลี่ยนแปลง",
  "update.failServer": "เซิร์ฟเวอร์อัปเดตไม่ตอบกลับในรูปแบบที่ใช้ได้ ไม่มีอะไรเปลี่ยนแปลง ({detail})",
  "update.failSignature": "ลายเซ็นของการอัปเดตไม่ตรง จึงปฏิเสธ ไม่ได้ติดตั้งอะไร",
  "update.failInstall": "ดาวน์โหลดการอัปเดตแล้วแต่ไม่ได้ติดตั้ง ({detail})",
  "update.failUnavailable": "รุ่นนี้ไม่มีการอัปเดต ({detail})",
  "update.failOther": "การตรวจหาการอัปเดตไม่เสร็จ ({detail})",
  "update.couldNotStart": "เริ่มไม่ได้",
  "update.unknownError": "ข้อผิดพลาดที่ไม่ทราบสาเหตุ",
} satisfies Catalogue<typeof enUpdate>;
