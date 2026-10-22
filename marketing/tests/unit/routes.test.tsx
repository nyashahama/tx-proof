import userEvent from "@testing-library/user-event";
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import AuditPage from "@/app/audit/page";
import CounterexamplePage from "@/app/counterexamples/commit-then-close/page";
import MethodPage from "@/app/method/page";
import ProductPage from "@/app/product/page";
import ResearchPage from "@/app/research/page";
import SafetyPage from "@/app/safety/page";

describe("supporting marketing routes", () => {
  it.each([
    [ProductPage, "Control the boundary. Preserve the evidence.", "Exactly five invariants"],
    [MethodPage, "Search the schedules your happy path never chooses.", "Compiled trace—not the seed"],
    [SafetyPage, "Nothing moves until the target proves it is disposable.", "Exit before mutation"],
    [ResearchPage, "Trust the method because every claim has a boundary.", "Research cut-off"],
    [CounterexamplePage, "One customer intent. Two fulfilments.", "evt_tiv_019"],
    [AuditPage, "A correctness audit for one money flow.", "Five approved invariants"],
  ])("gives its route a unique product job", (Page, heading, evidence) => {
    render(<Page />);

    expect(screen.getByRole("heading", { level: 1, name: heading })).toBeVisible();
    const evidenceMatches = screen.getAllByText(evidence, { exact: false });
    expect(evidenceMatches.length).toBeGreaterThan(0);
    expect(evidenceMatches[0]).toBeVisible();
  });

  it("keeps the canonical story fixed while the trace is minimized", async () => {
    const user = userEvent.setup();
    render(<CounterexamplePage />);
    expect(screen.queryByRole("tablist")).not.toBeInTheDocument();
    expect(screen.getByText("9 recorded events")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Minimize trace" }));
    expect(screen.getByRole("list", { name: "Minimized counterexample" }).children).toHaveLength(5);
    expect(screen.getByText("event evt_tiv_019 created 2 fulfilment rows")).toBeVisible();
  });

  it("offers an actionable public inquiry after qualification", () => {
    render(<AuditPage />);
    const inquiry = screen.getByRole("link", { name: /Open a public audit inquiry/i });
    expect(inquiry).toHaveAttribute("href", "https://github.com/nyashahama/tx-proof/issues/new");
  });

  it("keeps the counterexample and audit routes connected to the product story", () => {
    render(<ProductPage />);

    expect(screen.getByRole("link", { name: /Open the canonical counterexample/i })).toHaveAttribute(
      "href",
      "/counterexamples/commit-then-close",
    );
    expect(screen.getByRole("link", { name: /Qualify a repository/i })).toHaveAttribute(
      "href",
      "/audit",
    );
  });

  it("connects research claims to current primary authorities", () => {
    render(<ResearchPage />);

    expect(screen.getByRole("link", { name: /Stripe webhook delivery/i })).toHaveAttribute(
      "href",
      "https://docs.stripe.com/webhooks",
    );
    expect(screen.getByRole("link", { name: /PostgreSQL template databases/i })).toHaveAttribute(
      "href",
      "https://www.postgresql.org/docs/current/manage-ag-templatedbs.html",
    );
  });
});
