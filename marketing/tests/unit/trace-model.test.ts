import { describe, expect, it } from "vitest";

import {
  getScenario,
  minimizeScenario,
  scenarios,
} from "@/lib/trace/scenarios";

describe("transaction counterexample scenarios", () => {
  it("defines the four approved failure scenarios with stable identifiers", () => {
    expect(scenarios.map(({ id }) => id)).toEqual([
      "lost-response",
      "duplicate-webhook",
      "reordered-event",
      "process-crash",
    ]);
    expect(new Set(scenarios.map(({ id }) => id)).size).toBe(4);
  });

  it("keeps every scenario grounded in all four durable truth planes", () => {
    for (const scenario of scenarios) {
      expect(new Set(scenario.events.map(({ plane }) => plane))).toEqual(
        new Set(["intent", "provider", "database", "effect"]),
      );
      expect(scenario.events.some(({ tone }) => tone === "violation")).toBe(true);
    }
  });

  it("models the canonical commit-then-close witness from the blueprint", () => {
    const scenario = getScenario("lost-response");

    expect(scenario.invariant).toBe("webhook-effect-at-most-once");
    expect(scenario.witness).toBe(
      "event evt_tiv_019 created 2 fulfilment rows",
    );
    expect(scenario.reproduction).toBe("3/3 fresh baselines");
    expect(scenario.events.map(({ label }) => label)).toEqual(
      expect.arrayContaining([
        "Checkout requested",
        "PaymentIntent committed",
        "Response connection closed",
        "Success event delivered",
        "API process killed",
        "Event retried",
        "Fulfilment duplicated",
      ]),
    );
  });

  it("shrinks the canonical failure to the five decisive actions in order", () => {
    const minimized = minimizeScenario(getScenario("lost-response"));

    expect(minimized).toHaveLength(5);
    expect(minimized.map(({ label }) => label)).toEqual([
      "Checkout requested",
      "PaymentIntent committed; response closed",
      "Success event delivered",
      "API process killed",
      "Event retried",
    ]);
    expect(minimized.map(({ sequence }) => sequence)).toEqual([1, 2, 3, 4, 5]);
  });

  it("fails loudly for an unknown scenario instead of inventing a fallback", () => {
    expect(() => getScenario("network-magic")).toThrowError(
      "Unknown trace scenario: network-magic",
    );
  });
});
