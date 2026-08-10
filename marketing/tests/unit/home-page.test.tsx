import { render, screen } from "@testing-library/react";
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
      "Four systems can be individually right. Together, they can still be wrong.",
      "Declare the truth. Search the boundary. Keep the regression.",
      "One failure. Every artifact your team needs.",
      "Destructive by design. Safe by refusal.",
      "Know exactly what this is—and what it isn’t.",
      "Bring one money flow. Leave with an owned regression.",
    ];

    for (const name of sectionHeadings) {
      expect(screen.getByRole("heading", { level: 2, name })).toBeVisible();
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
