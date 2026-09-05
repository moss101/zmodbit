import type { ReactNode } from "react";

/** Raised panel with an optional header action slot. */
export function Panel({
  title,
  actions,
  children,
}: {
  title: string;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="modbit-panel">
      <header>
        <h3>{title}</h3>
        {actions ? <div className="modbit-panel-actions">{actions}</div> : null}
      </header>
      <div className="modbit-panel-body">{children}</div>
    </section>
  );
}
