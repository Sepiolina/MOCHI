import { useId } from "react";

import { t } from "../i18n";
import { SettingRow } from "./Row";
import { isMotionChoice, setMotion, useMotion } from "./motionChoice";

/**
 * Animations (owner request 2026-11-14): menus, windows and lists move smoothly, and can be
 * turned off on a slower computer or by preference. "As the system" follows the operating
 * system's reduce-motion setting. Applies at once in every window.
 */
export function Motion() {
  const motion = useMotion();
  const id = useId();
  const noteId = useId();
  return (
    <SettingRow
      title={t("settings.motionLabel")}
      titleFor={id}
      description={t("settings.motionShort")}
      descriptionId={noteId}
      more={t("settings.motionNote")}
    >
      <select
        id={id}
        data-testid="motion-select"
        aria-describedby={noteId}
        value={motion}
        onChange={(e) => {
          if (isMotionChoice(e.target.value)) setMotion(e.target.value);
        }}
      >
        <option value="system">{t("settings.motionSystem")}</option>
        <option value="on">{t("settings.motionOn")}</option>
        <option value="off">{t("settings.motionOff")}</option>
      </select>
    </SettingRow>
  );
}
