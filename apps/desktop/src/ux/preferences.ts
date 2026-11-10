// Local shell preferences. These are presentation-only and never substitute for
// domain state or server authorization.

import { shellCopy } from "./copy";

export type ThemePreference = "auto" | "light" | "dark";
export type DensityPreference = "comfortable" | "compact";
export type MotionPreference = "system" | "reduced";
export type MaterialPreference = "glass" | "solid";
export type AccentPreference = "cyan" | "blue" | "violet";
export type FontPreference = "system" | "yahei";

export interface ShellPreferences {
  theme: ThemePreference;
  density: DensityPreference;
  motion: MotionPreference;
  material: MaterialPreference;
  accent: AccentPreference;
  font: FontPreference;
}

export const DEFAULT_PREFERENCES: ShellPreferences = {
  theme: "auto",
  density: "comfortable",
  motion: "system",
  material: "glass",
  accent: "cyan",
  font: "system",
};

export const MATERIAL_OPTIONS: ReadonlyArray<{ value: MaterialPreference; label: string }> = [
  { value: "glass", label: "磨砂玻璃" },
  { value: "solid", label: "纯色" },
];
export const ACCENT_OPTIONS: ReadonlyArray<{ value: AccentPreference; label: string }> = [
  { value: "cyan", label: "晴空青" },
  { value: "blue", label: "湖水蓝" },
  { value: "violet", label: "暮光紫" },
];
export const FONT_OPTIONS: ReadonlyArray<{ value: FontPreference; label: string }> = [
  { value: "system", label: "系统字体" },
  { value: "yahei", label: "微软雅黑" },
];

export const THEME_OPTIONS: ReadonlyArray<{
  value: ThemePreference;
  label: string;
}> = [
  { value: "auto", label: shellCopy.themeAuto },
  { value: "light", label: shellCopy.themeLight },
  { value: "dark", label: shellCopy.themeDark },
];

export const DENSITY_OPTIONS: ReadonlyArray<{
  value: DensityPreference;
  label: string;
}> = [
  { value: "comfortable", label: shellCopy.densityComfortable },
  { value: "compact", label: shellCopy.densityCompact },
];

export const MOTION_OPTIONS: ReadonlyArray<{
  value: MotionPreference;
  label: string;
}> = [
  { value: "system", label: shellCopy.motionSystem },
  { value: "reduced", label: shellCopy.motionReduced },
];

const STORAGE_KEY = "shtmcl.shell.preferences.v1";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isTheme(value: unknown): value is ThemePreference {
  return value === "auto" || value === "light" || value === "dark";
}

function isDensity(value: unknown): value is DensityPreference {
  return value === "comfortable" || value === "compact";
}

function isMotion(value: unknown): value is MotionPreference {
  return value === "system" || value === "reduced";
}

export function loadPreferences(): ShellPreferences {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return { ...DEFAULT_PREFERENCES };
    }
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed)) {
      return { ...DEFAULT_PREFERENCES };
    }
    return {
      theme: isTheme(parsed.theme) ? parsed.theme : DEFAULT_PREFERENCES.theme,
      density: isDensity(parsed.density)
        ? parsed.density
        : DEFAULT_PREFERENCES.density,
      motion: isMotion(parsed.motion) ? parsed.motion : DEFAULT_PREFERENCES.motion,
      material: parsed.material === "solid" ? "solid" : DEFAULT_PREFERENCES.material,
      accent: parsed.accent === "blue" || parsed.accent === "violet"
        ? parsed.accent : DEFAULT_PREFERENCES.accent,
      font: parsed.font === "yahei" ? "yahei" : DEFAULT_PREFERENCES.font,
    };
  } catch {
    return { ...DEFAULT_PREFERENCES };
  }
}

export function savePreferences(preferences: ShellPreferences): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(preferences));
  } catch {
    // Private browsing or a locked profile must not break the shell.
  }
}

export function applyPreferences(preferences: ShellPreferences): void {
  const root = document.documentElement;
  root.dataset.theme = preferences.theme;
  root.dataset.density = preferences.density;
  root.dataset.motion = preferences.motion;
  root.dataset.material = preferences.material;
  root.dataset.accent = preferences.accent;
  root.dataset.font = preferences.font;
}
