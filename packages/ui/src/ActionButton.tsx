import type { ReactNode } from "react";

/** Primary/secondary/danger action button (token-styled). */
export function ActionButton({
  kind = "primary",
  onClick,
  disabled,
  children,
  title,
}: {
  kind?: "primary" | "secondary" | "danger";
  onClick?: () => void;
  disabled?: boolean;
  children: ReactNode;
  title?: string;
}) {
  return (
    <button
      type="button"
      className={`modbit-button modbit-button-${kind}`}
      onClick={onClick}
      disabled={disabled}
      title={title}
    >
      {children}
    </button>
  );
}
