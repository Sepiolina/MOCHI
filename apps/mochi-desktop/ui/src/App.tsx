import { Browser } from "./browser/Browser";
import { ForeignBrowser } from "./foreign/ForeignBrowser";
import { Help } from "./help/Help";
import { Settings } from "./settings/Settings";
import { useLocale } from "./i18n";

export function App() {
  // Re-render the whole tree when the language changes (no reload, no lost state).
  useLocale();
  return (
    <main>
      <Browser
        help={<Help />}
        foreign={<ForeignBrowser />}
        tools={{
          settings: <Settings />,
        }}
      />
    </main>
  );
}
