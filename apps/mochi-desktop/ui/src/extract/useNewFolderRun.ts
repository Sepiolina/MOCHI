import { useMemo, useRef } from "react";

/**
 * Whether the job that is running (or about to start) extracts into a folder
 * Rust just made for it ("Extract to..." with a new folder). If the job ends
 * without extracting anything, that folder is tidied up. Read by the job's
 * event handler, never by rendering, so it is kept outside state; `take` reads
 * it and clears it in one step. The returned object is stable.
 */
export function useNewFolderRun(): { set: () => void; take: () => boolean } {
  const ref = useRef(false);
  return useMemo(
    () => ({
      set: () => {
        ref.current = true;
      },
      take: () => {
        const was = ref.current;
        ref.current = false;
        return was;
      },
    }),
    [],
  );
}
