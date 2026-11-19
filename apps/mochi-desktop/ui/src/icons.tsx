// Small outline icons for the start screen and the file lists. Decorative: each
// is hidden from assistive technology (the button or the text beside it carries
// the name), draws with `currentColor`, and is plain SVG shapes, so no string
// from an archive or a disk can reach it. Which file-type icon a row gets is
// chosen in `browser/fileKind.ts` from a fixed table; nothing here reads a name.
import type { ReactNode } from "react";

function Icon({
  children,
  className = "icon",
  size = 20,
}: {
  children: ReactNode;
  className?: string;
  size?: number;
}) {
  return (
    <svg
      className={className}
      viewBox="0 0 24 24"
      width={size}
      height={size}
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {children}
    </svg>
  );
}

export function FolderIcon() {
  return (
    <Icon>
      <path d="M3 7.5a2 2 0 0 1 2-2h4.2l2 2.3H19a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
    </Icon>
  );
}

export function PlusIcon() {
  return (
    <Icon>
      <path d="M12 5v14M5 12h14" />
    </Icon>
  );
}

export function ExtractIcon() {
  return (
    <Icon>
      <path d="M12 15V4M8.5 7.5 12 4l3.5 3.5" />
      <path d="M5 13v5a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-5" />
    </Icon>
  );
}

export function GearIcon() {
  return (
    <Icon>
      <circle cx="12" cy="12" r="3" />
      <path d="M12 3v2.2M12 18.8V21M3 12h2.2M18.8 12H21M5.6 5.6l1.6 1.6M16.8 16.8l1.6 1.6M18.4 5.6l-1.6 1.6M7.2 16.8l-1.6 1.6" />
      <circle cx="12" cy="12" r="6.6" />
    </Icon>
  );
}

export function BackIcon() {
  return (
    <Icon>
      <path d="M19 12H5M11 6l-6 6 6 6" />
    </Icon>
  );
}

export function ForwardIcon() {
  return (
    <Icon>
      <path d="M5 12h14M13 6l6 6-6 6" />
    </Icon>
  );
}

export function UpIcon() {
  return (
    <Icon>
      <path d="M12 19V5M6 11l6-6 6 6" />
    </Icon>
  );
}

export function SearchIcon() {
  return (
    <Icon>
      <circle cx="11" cy="11" r="6.5" />
      <path d="m16 16 4.5 4.5" />
    </Icon>
  );
}

/** The caret beside a sorted column's name. */
export function CaretUpIcon() {
  return (
    <Icon className="icon caret" size={14}>
      <path d="m6 15 6-6 6 6" />
    </Icon>
  );
}

/**
 * The file-type icons of a listing row: a folder, an image, a music file, a
 * video, an archive, source code, a spreadsheet, a PDF, a text file, any other
 * file, and a link. Each is an outline with a faint
 * fill; the colour is the row's (`.file-icon-*` in the stylesheet).
 */
export type FileIconKind =
  | "folder"
  | "image"
  | "audio"
  | "video"
  | "archive"
  | "code"
  | "sheet"
  | "pdf"
  | "text"
  | "generic"
  | "link";

const PAGE = "M7 3h7l5 5v11a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z";
const FOLD = "M14 3v5h5";

export function FileTypeIcon({ kind }: { kind: FileIconKind }) {
  const cls = `icon file-icon file-icon-${kind}`;
  switch (kind) {
    case "folder":
      return (
        <Icon className={cls}>
          <path className="fill" d="M3 7.5a2 2 0 0 1 2-2h4.2l2 2.3H19a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
        </Icon>
      );
    case "image":
      return (
        <Icon className={cls}>
          <rect className="fill" x="4" y="4" width="16" height="16" rx="2.5" />
          <circle cx="9" cy="9.5" r="1.4" />
          <path d="m5 17 4.5-4.5 3 3L15 13l4 4" />
        </Icon>
      );
    case "audio":
      return (
        <Icon className={cls}>
          <path className="fill" d={PAGE} />
          <path d={FOLD} />
          <path d="M11 17.2V10.5l4.5-1.2v6.4" />
          <circle cx="9.6" cy="17.3" r="1.5" />
          <circle cx="14.1" cy="15.8" r="1.5" />
        </Icon>
      );
    case "video":
      return (
        <Icon className={cls}>
          <rect className="fill" x="3.5" y="5.5" width="17" height="13" rx="2.5" />
          <path d="m10.2 9.4 4.6 2.6-4.6 2.6z" />
        </Icon>
      );
    case "archive":
      return (
        <Icon className={cls}>
          <path className="fill" d={PAGE} />
          <path d={FOLD} />
          <path d="M11 3v2h1.5v2H11v2h1.5v2H11v2.5M10.2 14.5h2.6v3h-2.6z" />
        </Icon>
      );
    case "code":
      return (
        <Icon className={cls}>
          <path className="fill" d={PAGE} />
          <path d={FOLD} />
          <path d="m10 11.5-2.2 2.2 2.2 2.2M14 11.5l2.2 2.2-2.2 2.2" />
        </Icon>
      );
    case "sheet":
      return (
        <Icon className={cls}>
          <rect className="fill" x="4" y="4" width="16" height="16" rx="2.5" />
          <path d="M4 9.5h16M4 14.5h16M9.5 4v16M14.5 4v16" />
        </Icon>
      );
    case "pdf":
      return (
        <Icon className={cls}>
          <path className="fill" d={PAGE} />
          <path d={FOLD} />
          <path d="M9 17.5c1.5-1.2 3-3.6 3-5.8 0-.9-.9-.9-.9 0 0 1.9 2.2 3.8 3.9 4.2" />
        </Icon>
      );
    case "text":
      return (
        <Icon className={cls}>
          <path className="fill" d={PAGE} />
          <path d={FOLD} />
          <path d="M8.5 12.5h7M8.5 15.5h7M8.5 9.5h2.5" />
        </Icon>
      );
    case "link":
      return (
        <Icon className={cls}>
          <path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1" />
          <path d="M14 10a4 4 0 0 0-5.7 0l-3 3A4 4 0 0 0 11 18.7l1-1" />
        </Icon>
      );
    case "generic":
      return (
        <Icon className={cls}>
          <path className="fill" d={PAGE} />
          <path d={FOLD} />
        </Icon>
      );
  }
}

export function CheckIcon() {
  return (
    <Icon>
      <path d="M12 3.5 5 6v5.5c0 4.2 2.8 7.4 7 9 4.2-1.6 7-4.8 7-9V6z" />
      <path d="m8.8 12 2.4 2.4 4.2-4.6" />
    </Icon>
  );
}

/** A plain tick: "this one" in a list (the version on screen). Not the Test shield, so it
    never reads as a verification. */
export function TickIcon() {
  return (
    <Icon>
      <path d="m5 12.5 4.5 4.5L19 7.5" />
    </Icon>
  );
}

/** A bug under a lens: the virus scan. */
export function ScanIcon() {
  return (
    <Icon>
      <rect x="7" y="8" width="8" height="10" rx="4" />
      <path d="M11 8v10M7 12H4.5M7 15.5l-2.5 1.5M15 12h2.5M9 8.2 7.5 6M13 8.2 14.5 6" />
      <circle cx="17.5" cy="17.5" r="2.5" />
      <path d="m19.3 19.3 1.7 1.7" />
    </Icon>
  );
}

export function MinusIcon() {
  return (
    <Icon>
      <path d="M5 12h14" />
    </Icon>
  );
}

export function InfoIcon() {
  return (
    <Icon>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 11v5.2M12 7.8h.01" />
    </Icon>
  );
}

export function MoreIcon() {
  return (
    <Icon>
      <circle cx="6" cy="12" r="1.2" />
      <circle cx="12" cy="12" r="1.2" />
      <circle cx="18" cy="12" r="1.2" />
    </Icon>
  );
}

export function HelpIcon() {
  return (
    <Icon>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M9.6 9.6a2.5 2.5 0 1 1 3.6 2.2c-.8.4-1.2 1-1.2 1.8M12 16.6h.01" />
    </Icon>
  );
}

/** A folder with an arrow into it: Extract to... */
export function ExtractToIcon() {
  return (
    <Icon>
      <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
      <path d="M12 9.5v6M9.6 13.1 12 15.5l2.4-2.4" />
    </Icon>
  );
}

export function FoldersIcon() {
  return (
    <Icon size={18}>
      <rect x="4" y="5" width="16" height="14" rx="2" />
      <path d="M9.5 5v14" />
    </Icon>
  );
}

export function CloseIcon() {
  return (
    <Icon size={16}>
      <path d="M6 6l12 12M18 6 6 18" />
    </Icon>
  );
}

export function CaretDownIcon() {
  return (
    <Icon className="icon caret" size={14}>
      <path d="m6 9 6 6 6-6" />
    </Icon>
  );
}

export function OpenIcon() {
  return (
    <Icon>
      <path d="M3.5 18.5V6.5a2 2 0 0 1 2-2h3.7l2 2.2H17a2 2 0 0 1 2 2v1.8" />
      <path d="M3.5 18.5 6.2 11a1.5 1.5 0 0 1 1.4-1h12.6a1 1 0 0 1 .9 1.4l-2.6 6.4a1.5 1.5 0 0 1-1.4.9H3.5z" />
    </Icon>
  );
}

export function NewArchiveIcon() {
  return (
    <Icon>
      <path d="M13 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V9z" />
      <path d="M13 3v6h6M12 12v6M9 15h6" />
    </Icon>
  );
}

export function TrashIcon() {
  return (
    <Icon>
      <path d="M4.5 7h15M9.5 7V4.8h5V7M6.5 7l.9 12.2a1.5 1.5 0 0 0 1.5 1.3h6.2a1.5 1.5 0 0 0 1.5-1.3L17.5 7M10 10.5v6.5M14 10.5v6.5" />
    </Icon>
  );
}

export function KeyIcon() {
  return (
    <Icon>
      <circle cx="8" cy="15" r="4" />
      <path d="m11 12 8.5-8.5M16 7l2.5 2.5M14 9l2 2" />
    </Icon>
  );
}

export function WrenchIcon() {
  return (
    <Icon>
      <path d="M14.5 6.5a4 4 0 0 0 5 5l-8 8a2.1 2.1 0 0 1-3-3l8-8a4 4 0 0 1-2-2z" />
      <path d="m17.5 3.5-3 3 3 3 3-3" />
    </Icon>
  );
}

export function ShowFolderIcon() {
  return (
    <Icon size={16}>
      <path d="M3 7.5a2 2 0 0 1 2-2h4.2l2 2.3H19a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
      <path d="M10 15.5h5M13 13l2.5 2.5L13 18" />
    </Icon>
  );
}

export function RefreshIcon() {
  return (
    <Icon size={18}>
      <path d="M19 12a7 7 0 1 1-2.05-4.95M19 4.5V8h-3.5" />
    </Icon>
  );
}

/** A folder with a magnifier: MOCHI Find (names inside many archives). */
export function FindIcon() {
  return (
    <Icon>
      <path d="M3.5 17.5v-11a2 2 0 0 1 2-2h3.7l2 2.2H17a2 2 0 0 1 2 2v1.6" />
      <path d="M3.5 17.5a2 2 0 0 0 2 2h5" />
      <circle cx="16" cy="15.5" r="3.6" />
      <path d="m18.6 18.1 2.6 2.6" />
    </Icon>
  );
}

/**
 * The title bar's window buttons, drawn as the thin 10-pixel glyphs of a
 * Windows 11 caption (the same on Ubuntu). Decorative: the button is named.
 */
function Glyph({ children }: { children: ReactNode }) {
  return (
    <svg
      className="caption-glyph"
      viewBox="0 0 10 10"
      width={10}
      height={10}
      fill="none"
      stroke="currentColor"
      strokeWidth="1"
      aria-hidden="true"
      focusable="false"
      shapeRendering="crispEdges"
    >
      {children}
    </svg>
  );
}

export function MinimizeGlyph() {
  return (
    <Glyph>
      <path d="M0 5.5h10" />
    </Glyph>
  );
}

export function MaximizeGlyph() {
  return (
    <Glyph>
      <rect x="0.5" y="0.5" width="9" height="9" />
    </Glyph>
  );
}

export function RestoreGlyph() {
  return (
    <Glyph>
      <rect x="0.5" y="2.5" width="7" height="7" />
      <path d="M2.5 2.5v-2h7v7h-2" />
    </Glyph>
  );
}

export function CloseGlyph() {
  return (
    <Glyph>
      <path d="M0.5 0.5l9 9M9.5 0.5l-9 9" />
    </Glyph>
  );
}

/** Two arrows in opposite directions: a transfer in progress. */
export function TransferIcon() {
  return (
    <Icon size={18}>
      <path d="M4 8h14M14.5 4.5 18 8l-3.5 3.5" />
      <path d="M20 16H6M9.5 12.5 6 16l3.5 3.5" />
    </Icon>
  );
}

export function PauseIcon() {
  return (
    <Icon>
      <path d="M9 6v12M15 6v12" strokeWidth="2.4" />
    </Icon>
  );
}

export function PlayIcon() {
  return (
    <Icon>
      <path d="M8 5.5v13l10.5-6.5z" />
    </Icon>
  );
}

/** A circle with an exclamation mark. */
export function ErrorIcon() {
  return (
    <Icon size={16}>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 7.8v5M12 16.2h.01" />
    </Icon>
  );
}
