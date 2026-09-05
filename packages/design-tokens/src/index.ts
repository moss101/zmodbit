/**
 * Design tokens for the Modbit desktop surface (docs/32 § visual language).
 * Single source of truth: TS constants plus the equivalent CSS custom
 * properties (`TOKEN_STYLE`), injected once at renderer boot so the desktop
 * and any future web surface share one token set.
 */

export const DESIGN_TOKENS_PACKAGE_NAME = "@modbit/design-tokens";

/** Neutral surface ramp (dark-first workspace). */
export const color = {
  bgBase: "#0e1116",
  bgPanel: "#161b22",
  bgPanelRaised: "#1c232c",
  borderSubtle: "#2a323d",
  borderStrong: "#3d4753",
  textPrimary: "#e6edf3",
  textSecondary: "#9aa7b4",
  textMuted: "#6b7784",
} as const;

/** Task/step status colors (keys map to task and step state names). */
export const colorStatus = {
  created: "#8b949e",
  queued: "#8b949e",
  running: "#3d7eff",
  streaming: "#3d7eff",
  executing: "#58a6ff",
  verifying: "#a371f7",
  readyForReview: "#d29922",
  waiting: "#d29922",
  completed: "#3fb950",
  passed: "#3fb950",
  failed: "#f85149",
  cancelled: "#8b949e",
  interrupted: "#f85149",
  prepared: "#8b949e",
} as const;

export type StatusToken = keyof typeof colorStatus;

/** Accent + semantic feedback. */
export const colorAccent = {
  primary: "#3d7eff",
  primaryHover: "#5c93ff",
  danger: "#f85149",
  success: "#3fb950",
} as const;

/** 4px-base spacing scale. */
export const space = {
  xs: "4px",
  sm: "8px",
  md: "12px",
  lg: "16px",
  xl: "24px",
  xxl: "32px",
} as const;

/** Typography scale (system stack; no webfont dependency). */
export const font = {
  family: "-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', sans-serif",
  familyMono: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
  sizeXs: "11px",
  sizeSm: "13px",
  sizeMd: "14px",
  sizeLg: "17px",
  sizeXl: "22px",
  weightRegular: 400,
  weightMedium: 500,
  weightStrong: 600,
} as const;

export const radius = {
  sm: "4px",
  md: "8px",
  lg: "12px",
  pill: "999px",
} as const;

/** Z-order contract for layered surfaces. */
export const zIndex = {
  base: 0,
  raised: 10,
  overlay: 100,
  toast: 1000,
} as const;

/** CSS custom properties for the injected stylesheet (`TOKEN_STYLE`). */
export const tokenVars = {
  "--modbit-bg-base": color.bgBase,
  "--modbit-bg-panel": color.bgPanel,
  "--modbit-bg-panel-raised": color.bgPanelRaised,
  "--modbit-border-subtle": color.borderSubtle,
  "--modbit-border-strong": color.borderStrong,
  "--modbit-text-primary": color.textPrimary,
  "--modbit-text-secondary": color.textSecondary,
  "--modbit-text-muted": color.textMuted,
  "--modbit-accent": colorAccent.primary,
  "--modbit-accent-hover": colorAccent.primaryHover,
  "--modbit-danger": colorAccent.danger,
  "--modbit-success": colorAccent.success,
  "--modbit-space-xs": space.xs,
  "--modbit-space-sm": space.sm,
  "--modbit-space-md": space.md,
  "--modbit-space-lg": space.lg,
  "--modbit-space-xl": space.xl,
  "--modbit-radius-sm": radius.sm,
  "--modbit-radius-md": radius.md,
  "--modbit-radius-lg": radius.lg,
  "--modbit-font-mono": font.familyMono,
} as const;

/** Stylesheet text injected once at renderer boot. */
export const TOKEN_STYLE: string = [
  ":root {",
  ...Object.entries(tokenVars).map(([k, v]) => `  ${k}: ${v};`),
  "}",
].join("\n");

/**
 * Status color for a task or step state string. Accepts wire forms
 * ("ready_for_review", "exited(0)", "tool_use", camelCase tokens); unknown
 * values resolve to the neutral created color.
 */
export function statusColor(state: string): string {
  const camel = state
    .replace(/[_()-]/g, "")
    .replace(/^[A-Z]/, (c) => c.toLowerCase())
    .replace(/[A-Z]/g, (c) => c.toLowerCase() + "");
  const normalized = state.replace(/[^a-zA-Z]/g, "").toLowerCase();
  const byKey = (colorStatus as Record<string, string>)[normalized];
  if (byKey) return byKey;
  if (camel === "readyforreview") return colorStatus.readyForReview;
  if (normalized.startsWith("exited")) return colorStatus.completed;
  if (normalized.includes("fail") || normalized.includes("interrupt")) {
    return colorStatus.failed;
  }
  if (normalized.includes("wait")) return colorStatus.waiting;
  return colorStatus.created;
}
