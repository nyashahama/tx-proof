import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { TraceInstrument } from "@/components/hero/trace-instrument";

describe("TraceInstrument", () => {
  it("renders the canonical lost-response witness as the meaningful default", () => {
    render(<TraceInstrument />);

    expect(
      screen.getByRole("heading", {
        level: 2,
        name: "Committed remotely. Unknown locally.",
      }),
    ).toBeVisible();
    expect(screen.getByText("webhook-effect-at-most-once")).toBeVisible();
    expect(
      screen.getByText("event evt_tiv_019 created 2 fulfilment rows"),
    ).toBeVisible();
    expect(screen.getByRole("tab", { name: "01 Lost response" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
  });

  it("switches scenarios from an accessible tab list and announces the result", async () => {
    const user = userEvent.setup();
    render(<TraceInstrument />);

    await user.click(screen.getByRole("tab", { name: "02 Duplicate webhook" }));

    expect(
      screen.getByRole("heading", { level: 2, name: "One event. Two durable effects." }),
    ).toBeVisible();
    expect(
      screen.getByText("event evt_tiv_024 mapped to effects f_311 and f_312"),
    ).toBeVisible();
    expect(screen.getByRole("status")).toHaveTextContent(
      "Duplicate webhook scenario selected. Invariant webhook-effect-at-most-once violated.",
    );
  });

  it("reveals the five decisive actions without hiding the failure evidence", async () => {
    const user = userEvent.setup();
    render(<TraceInstrument />);

    await user.click(screen.getByRole("button", { name: "Minimize trace" }));

    const minimized = screen.getByRole("list", { name: "Minimized counterexample" });
    expect(within(minimized).getAllByRole("listitem")).toHaveLength(5);
    expect(screen.getByText("5 decisive actions")).toBeVisible();
    expect(
      screen.getByText("event evt_tiv_019 created 2 fulfilment rows"),
    ).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Show full trace" }));
    expect(screen.getByText("9 recorded events")).toBeVisible();
  });
});
