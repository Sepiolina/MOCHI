import type { Catalogue } from "../types";
import type { help as enHelp } from "../en/help";
// Help, User guide (audit F8). The guide is the command-line guide from docs/user, bundled
// with the installer and opened offline; the labels say so before it is opened.
export const help = {
  "help.heading": "ความช่วยเหลือ",
  "help.toolbar": "ช่วยเหลือ",
  "help.keys":
    "ในรายการ ใช้ปุ่มลูกศรเลื่อนระหว่างรายการ Page Up, Page Down, Home และ End เพื่อข้ามไปไกล Space เพื่อเลือก และ Enter เพื่อเปิดโฟลเดอร์",
  "help.shortcuts":
    "Ctrl+O เปิดไฟล์บีบอัด, Ctrl+F ค้นหาชื่อในไฟล์บีบอัด, Ctrl+A เลือกทั้งโฟลเดอร์, Escape ยกเลิกการเลือก, Backspace ขึ้นไปหนึ่งระดับ, Alt+ลูกศรซ้ายและ Alt+ลูกศรขวา ย้อนกลับและไปข้างหน้าตามโฟลเดอร์ที่เปิดไว้ และ Delete นำรายการที่เลือกออก กด Ctrl หรือ Shift ค้างพร้อมคลิกเพื่อเลือกหลายรายการ ลากรายการออกจากรายการไปที่โฟลเดอร์เพื่อแตกไฟล์ และวางไฟล์ลงในหน้าต่างเพื่อเพิ่ม กด Ctrl+Shift+F เพื่อค้นหาชื่อภายในไฟล์บีบอัดหลายไฟล์ในโฟลเดอร์ที่คุณเลือก กด Ctrl+N เพื่อสร้างไฟล์บีบอัดใหม่ Ctrl+E เพื่อแตกไฟล์ Alt+Enter เพื่อแสดงหรือซ่อนรายละเอียด และ F1 เพื่อเปิดความช่วยเหลือนี้ คำสั่งเหล่านี้และคำสั่งอื่นอยู่ในเมนูเพิ่มเติมด้วย",
  "help.userGuide": "คู่มือผู้ใช้ (บรรทัดคำสั่ง)",
  "help.userGuideNote":
    "เขียนไว้สำหรับคำสั่ง mochi ในบรรทัดคำสั่ง ไม่ใช่สำหรับแอปนี้ บรรทัดแรก ๆ ชี้ไปยังหน้าจอที่ตรงกัน เปิดสำเนาที่ติดตั้งมากับ MOCHI ในเว็บเบราว์เซอร์เริ่มต้นของคุณ และไม่ต้องใช้เครือข่าย",
  "help.requested": "ส่งคู่มือผู้ใช้ให้โปรแกรมเริ่มต้นสำหรับเว็บเพจของคุณแล้ว",
  "help.failed": "เปิดคู่มือผู้ใช้ไม่สำเร็จ ({code}): {message}",
  "help.failedPlain": "เปิดคู่มือผู้ใช้ไม่สำเร็จ",
} satisfies Catalogue<typeof enHelp>;
