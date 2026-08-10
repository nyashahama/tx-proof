import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import Home from "@/app/page";

describe("marketing homepage", () => {
  it("states the product and its honest boundary above the fold", () => {
    render(<Home />);

    expect(
      screen.getByRole("heading", {
        level: 1,
        name: "Find the schedule that makes your database lie about money.",
      }),
    ).toBeVisible();
    expect(screen.getByText("Counterexample search, not proof.")).toBeVisible();
    expect(screen.getByRole("link", { name: "View a failing trace" })).toHaveAttribute(
      "href",
      "/counterexamples/commit-then-close",
    );
    expect(screen.getByRole("link", { name: "Book a correctness audit" })).toHaveAttribute(
      "href",
      "/audit",
    );
  });

  it("provides a complete technical story before the audit conversion", () => {
    render(<Home />);

    const sectionHeadings = [
      "How TxProof turns one intent into an owned regression",
      "From one intent to an owned counterexample.",
      "Four systems can be individually right. Together, they can still be wrong.",
      "One failure. Every artifact your team needs.",
      "Destructive by design. Safe by refusal.",
      "Know exactly what this is—and what it isn’t.",
      "Bring one money flow. Leave with an owned regression.",
    ];

    for (const name of sectionHeadings) {
      expect(screen.getByRole("heading", { level: 2, name })).toBeVisible();
    }
  });

  it("opens with three visual capabilities before the live counterexample", () => {
    render(<Home />);

    const capabilities = screen.getByRole("region", {
      name: "How TxProof turns one intent into an owned regression",
    });

    for (const name of [
      "Model the schedule",
      "Interrogate durable truth",
      "Own the regression",
    ]) {
      expect(within(capabilities).getByRole("heading", { level: 3, name })).toBeVisible();
    }

    expect(within(capabilities).getAllByRole("listitem")).toHaveLength(3);
    expect(
      screen.getByRole("heading", {
        level: 2,
        name: "From one intent to an owned counterexample.",
      }),
    ).toBeVisible();
  });

  it("states the operating principle without borrowing social proof", () => {
    render(<Home />);

    expect(
      screen.getByRole("heading", {
        level: 2,
        name: "Your money flow stays local. The failure becomes evidence your team can keep.",
      }),
    ).toBeVisible();
    expect(screen.queryByText(/trusted by/i)).not.toBeInTheDocument();
  });

  it("makes every evidence claim inspectable and labels the canonical fixture", () => {
    render(<Home />);

    const evidence = screen.getByRole("complementary", { name: "Product evidence" });
    expect(within(evidence).getAllByRole("link")).toHaveLength(5);
    expect(within(evidence).getByText("Modeled fixture · 3 / 3")).toBeVisible();
    expect(screen.getByText("INTERACTIVE MODELED EVIDENCE")).toBeVisible();

    for (const href of [
      "/safety",
      "/product",
      "/method",
      "/counterexamples/commit-then-close",
      "/audit",
    ]) {
      expect(evidence.querySelector(`a[href="${href}"]`)).toBeInTheDocument();
    }
  });

  it("links every primary research and product route", () => {
    render(<Home />);

    for (const href of [
      "/product",
      "/method",
      "/safety",
      "/research",
      "/counterexamples/commit-then-close",
      "/audit",
    ]) {
      expect(document.querySelector(`a[href="${href}"]`)).toBeInTheDocument();
    }
  });
});
