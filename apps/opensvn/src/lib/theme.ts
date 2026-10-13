export type ThemeChoice = "system" | "light" | "dark";

const KEY = "opensvn.theme";

export function readTheme(): ThemeChoice {
  try {
    const v = localStorage.getItem(KEY);
    return v === "light" || v === "dark" ? v : "system";
  } catch {
    return "system";
  }
}

/** Apply a theme: `system` removes the override so prefers-color-scheme decides (see tokens.css). */
export function applyTheme(theme: ThemeChoice): void {
  const root = document.documentElement;
  if (theme === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", theme);
  try {
    localStorage.setItem(KEY, theme);
  } catch {
    // Storage may be unavailable (private mode); the theme still applies for this session.
  }
}
