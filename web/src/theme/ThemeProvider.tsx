import * as React from "react";

export type ThemeName = "device" | "light" | "dark";

const STORAGE_KEY = "pico-link-web-theme";
const THEMES: ThemeName[] = ["device", "light", "dark"];

interface ThemeContextValue {
  theme: ThemeName;
  setTheme: (theme: ThemeName) => void;
}

const ThemeContext = React.createContext<ThemeContextValue | null>(null);

function readInitialTheme(): ThemeName {
  if (typeof window === "undefined") return "device";
  const stored = window.localStorage.getItem(STORAGE_KEY);
  return THEMES.includes(stored as ThemeName) ? (stored as ThemeName) : "device";
}

/**
 * Applies one of three CSS-variable themes (`data-theme` on `<html>`):
 * "device" (default, derived from core/src/render/theme.rs's palette --
 * see index.css's comment), "light", "dark". Andreas's 2026-09-26 DECISION
 * on pico-link-jyhk.10.
 */
export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [theme, setThemeState] = React.useState<ThemeName>(readInitialTheme);

  React.useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    window.localStorage.setItem(STORAGE_KEY, theme);
  }, [theme]);

  const setTheme = React.useCallback((next: ThemeName) => setThemeState(next), []);

  const value = React.useMemo(() => ({ theme, setTheme }), [theme, setTheme]);

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

export function useTheme(): ThemeContextValue {
  const ctx = React.useContext(ThemeContext);
  if (!ctx) throw new Error("useTheme must be used inside a ThemeProvider");
  return ctx;
}

export const THEME_NAMES = THEMES;
