import type { PrimaryRoute } from "@/lib/site";

export type OpenGraphCard = {
  sequence: string;
  eyebrow: string;
  title: string;
  detail: string;
};

export const openGraphCards: Record<PrimaryRoute, OpenGraphCard> = {
  "/": {
    sequence: "00",
    eyebrow: "BOUNDED COUNTEREXAMPLE SEARCH",
    title: "Find the schedule that makes your database lie about money.",
    detail: "Stripe · PostgreSQL · webhooks · retries · crashes",
  },
  "/product": {
    sequence: "01",
    eyebrow: "PRODUCT CONTRACT",
    title: "Control the boundary. Preserve the evidence.",
    detail: "Five invariants · fresh replay · one CI artifact",
  },
  "/method": {
    sequence: "02",
    eyebrow: "SEARCH METHOD",
    title: "Compile the schedules your happy path never chooses.",
    detail: "Compile · execute · check · replay · shrink",
  },
  "/safety": {
    sequence: "03",
    eyebrow: "SAFETY BOUNDARY",
    title: "Nothing moves until the target proves it is disposable.",
    detail: "Local only · synthetic data · bounded resources",
  },
  "/research": {
    sequence: "04",
    eyebrow: "SOURCE LEDGER",
    title: "Trust the method because every claim has a boundary.",
    detail: "Primary sources · dated review · prohibited guarantees",
  },
  "/counterexamples/commit-then-close": {
    sequence: "05",
    eyebrow: "CANONICAL COUNTEREXAMPLE",
    title: "One customer intent. Two fulfilments.",
    detail: "evt_tiv_019 · 3/3 replay · 5 decisive actions",
  },
  "/audit": {
    sequence: "06",
    eyebrow: "MONEY CORRECTNESS AUDIT",
    title: "A correctness audit for one money flow.",
    detail: "Fixed scope · local execution · engineering handoff",
  },
};
