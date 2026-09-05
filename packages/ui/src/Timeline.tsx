import { StatusPill } from "./StatusPill";

/** One timeline row: label, optional state pill, optional detail. */
export interface TimelineEntry {
  id: string;
  label: string;
  state?: string;
  detail?: string;
}

/** Vertical run timeline (turns and their steps). */
export function Timeline({ entries }: { entries: TimelineEntry[] }) {
  if (entries.length === 0) {
    return <p className="modbit-empty">no run activity yet</p>;
  }
  return (
    <ol className="modbit-timeline">
      {entries.map((entry) => (
        <li key={entry.id} className="modbit-timeline-entry">
          <span className="modbit-timeline-dot" data-state={entry.state ?? ""} />
          <span className="modbit-timeline-label">{entry.label}</span>
          {entry.state ? <StatusPill state={entry.state} /> : null}
          {entry.detail ? (
            <span className="modbit-timeline-detail">{entry.detail}</span>
          ) : null}
        </li>
      ))}
    </ol>
  );
}
