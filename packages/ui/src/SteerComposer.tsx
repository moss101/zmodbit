import { useState } from "react";
import { ActionButton } from "./ActionButton";

/** Steering input: durable control note sent to a running task. */
export function SteerComposer({
  onSteer,
  disabled,
}: {
  onSteer: (note: string) => void;
  disabled?: boolean;
}) {
  const [note, setNote] = useState("");
  const submit = () => {
    const trimmed = note.trim();
    if (!trimmed) return;
    onSteer(trimmed);
    setNote("");
  };
  return (
    <div className="modbit-steer">
      <input
        value={note}
        placeholder="steer the task…"
        onChange={(e) => setNote(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") submit();
        }}
        disabled={disabled}
      />
      <ActionButton kind="secondary" onClick={submit} disabled={disabled || !note.trim()}>
        Steer
      </ActionButton>
    </div>
  );
}
