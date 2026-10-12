import { useState } from "react";
import { NavLink, Route, Routes } from "react-router";
import { Library, Mic, Settings, Boxes, FileText, Users } from "lucide-react";
import { cn } from "./lib/utils";
import ErrorBoundary from "./components/ErrorBoundary";
import RecordControl from "./components/RecordControl";
import PermissionsBanner from "./components/PermissionsBanner";
import PermissionsSetup from "./components/PermissionsSetup";
import ModelsPage from "./features/models/ModelsPage";
import LibraryPage from "./features/library/LibraryPage";
import SettingsPage from "./features/settings/SettingsPage";
import MeetingPage from "./features/meeting/MeetingPage";
import TemplatesPage from "./features/templates/TemplatesPage";
import SpeakersPage from "./features/speakers/SpeakersPage";

const nav = [
  { to: "/", label: "Library", icon: Library },
  { to: "/templates", label: "Templates", icon: FileText },
  { to: "/speakers", label: "People", icon: Users },
  { to: "/models", label: "Models", icon: Boxes },
  { to: "/settings", label: "Settings", icon: Settings },
];

export default function App() {
  // While the setup card is up, the banner would only repeat it.
  const [setupVisible, setSetupVisible] = useState(false);
  return (
    <div className="flex h-full">
      <aside className="flex w-56 shrink-0 flex-col border-r border-neutral-200 bg-neutral-100/70 dark:border-neutral-800 dark:bg-neutral-900/60">
        <div data-tauri-drag-region className="flex h-12 items-center gap-2 px-4 pt-6 pb-2 text-sm font-semibold select-none">
          <Mic className="size-4 text-amber-600" /> Muesli
        </div>
        <nav className="mt-2 flex flex-col gap-0.5 px-2">
          {nav.map(({ to, label, icon: Icon }) => (
            <NavLink
              key={to}
              to={to}
              end={to === "/"}
              className={({ isActive }) =>
                cn(
                  "flex items-center gap-2 rounded-md px-2 py-1.5 text-sm text-neutral-700 hover:bg-neutral-200/70 dark:text-neutral-300 dark:hover:bg-neutral-800",
                  isActive && "bg-neutral-200 font-medium text-neutral-900 dark:bg-neutral-800 dark:text-white",
                )
              }
            >
              <Icon className="size-4" /> {label}
            </NavLink>
          ))}
        </nav>
        <RecordControl />
      </aside>
      <main className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <div data-tauri-drag-region className="h-7 w-full shrink-0" />
        <PermissionsSetup onVisibleChange={setSetupVisible} />
        <PermissionsBanner hidden={setupVisible} />
        <div className="min-h-0 flex-1 overflow-auto">
          <ErrorBoundary>
            <Routes>
              <Route path="/" element={<LibraryPage />} />
              <Route path="/meetings/:id" element={<MeetingPage />} />
              <Route path="/templates" element={<TemplatesPage />} />
              <Route path="/speakers" element={<SpeakersPage />} />
              <Route path="/models" element={<ModelsPage />} />
              <Route path="/settings" element={<SettingsPage />} />
            </Routes>
          </ErrorBoundary>
        </div>
      </main>
    </div>
  );
}
