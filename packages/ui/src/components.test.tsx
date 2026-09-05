/**
 * Component tests: rendering contracts only (labels, counts, disabled
 * behavior) — visual tokens live in @modbit/design-tokens.
 */
import { describe, expect, it } from "vitest";
import { render, fireEvent } from "@testing-library/react";
import {
  ActionButton,
  DiffFileRow,
  StatusPill,
  SteerComposer,
  Timeline,
  statusLabel,
} from "./index";

describe("StatusPill", () => {
  it("renders the wire state as a human label", () => {
    const { getByTitle } = render(<StatusPill state="ready_for_review" />);
    expect(getByTitle("ready_for_review").textContent).toBe("ready for review");
  });

  it("strips exit-code parentheses", () => {
    expect(statusLabel("exited(0)")).toBe("exited0");
  });
});

describe("Timeline", () => {
  it("lists entries in order with state pills", () => {
    const { getByText } = render(
      <Timeline
        entries={[
          { id: "t1", label: "Turn 1", state: "completed" },
          { id: "s1", label: "model_invoke", state: "completed", detail: "1.2s" },
        ]}
      />,
    );
    expect(getByText("Turn 1")).toBeTruthy();
    expect(getByText("model_invoke")).toBeTruthy();
    expect(getByText("1.2s")).toBeTruthy();
  });

  it("shows the empty state", () => {
    const { getByText } = render(<Timeline entries={[]} />);
    expect(getByText("no run activity yet")).toBeTruthy();
  });
});

describe("DiffFileRow", () => {
  it("shows path with numstat", () => {
    const { getByText } = render(
      <DiffFileRow path="src/lib.rs" additions={12} deletions={3} />,
    );
    expect(getByText("src/lib.rs")).toBeTruthy();
    expect(getByText("+12")).toBeTruthy();
    expect(getByText("−3")).toBeTruthy();
  });
});

describe("SteerComposer", () => {
  it("sends trimmed notes on Enter and clears", () => {
    const sent: string[] = [];
    const { getByPlaceholderText } = render(
      <SteerComposer onSteer={(n) => sent.push(n)} />,
    );
    const input = getByPlaceholderText("steer the task…") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "  also add docs  " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(sent).toEqual(["also add docs"]);
    expect(input.value).toBe("");
  });
});

describe("ActionButton", () => {
  it("blocks clicks when disabled", () => {
    let clicked = 0;
    const { getByRole } = render(
      <ActionButton onClick={() => clicked++} disabled>
        Go
      </ActionButton>,
    );
    fireEvent.click(getByRole("button"));
    expect(clicked).toBe(0);
  });
});
