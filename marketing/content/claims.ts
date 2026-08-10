export type SourceRecord = {
  id: string;
  authority: string;
  topic: string;
  evidence: string;
  status: "primary" | "prior-art" | "project";
  url: string;
  public: boolean;
};

export type ClaimRecord = {
  id: string;
  copy: string;
  scope: string;
  sourceIds: string[];
  reviewedAt: string;
  expiresAt?: string;
  status: "approved" | "hypothesis" | "prohibited";
};

export const sourceRecords: SourceRecord[] = [
  {
    id: "S01",
    authority: "Stripe",
    topic: "Webhook delivery",
    evidence: "Duplicate events, unordered delivery, and retry behavior",
    status: "primary",
    url: "https://docs.stripe.com/webhooks",
    public: true,
  },
  {
    id: "S02",
    authority: "Stripe",
    topic: "Indeterminate outcomes",
    evidence: "Network failure after provider-side execution",
    status: "primary",
    url: "https://docs.stripe.com/error-low-level",
    public: true,
  },
  {
    id: "S21",
    authority: "FoundationDB",
    topic: "Simulation testing",
    evidence: "Generated workloads, fault injection, and invariants",
    status: "prior-art",
    url: "https://apple.github.io/foundationdb/testing.html",
    public: true,
  },
  {
    id: "S23",
    authority: "Jepsen",
    topic: "History analysis",
    evidence: "Counterexamples grounded in observable operations",
    status: "prior-art",
    url: "https://jepsen.io/consistency/models",
    public: true,
  },
  {
    id: "S26",
    authority: "fast-check",
    topic: "Model-based shrinking",
    evidence: "State-machine generation and minimized failures",
    status: "prior-art",
    url: "https://github.com/dubzzz/fast-check",
    public: true,
  },
  {
    id: "S38",
    authority: "PostgreSQL",
    topic: "Template databases",
    evidence: "Fresh disposable database baseline mechanics",
    status: "primary",
    url: "https://www.postgresql.org/docs/current/manage-ag-templatedbs.html",
    public: true,
  },
  {
    id: "B01",
    authority: "TxProof blueprint",
    topic: "Product semantics",
    evidence: "Bounded verdict language and canonical modeled counterexample",
    status: "project",
    url: "/research",
    public: false,
  },
];

export const claimRecords: ClaimRecord[] = [
  {
    id: "CL-001",
    copy: "Stripe webhook endpoints can receive duplicate events and events out of order.",
    scope: "Stripe webhook delivery behavior documented by Stripe",
    sourceIds: ["S01"],
    reviewedAt: "2026-08-10",
    expiresAt: "2026-11-10",
    status: "approved",
  },
  {
    id: "CL-002",
    copy: "A network failure can leave a client without a conclusive result after an API request begins.",
    scope: "Stripe API indeterminate-outcome guidance",
    sourceIds: ["S02"],
    reviewedAt: "2026-08-10",
    expiresAt: "2026-11-10",
    status: "approved",
  },
  {
    id: "CL-014",
    copy: "Passing means no violation found under this model and budget.",
    scope: "One configured TxProof campaign; excludes proof of correctness",
    sourceIds: ["B01"],
    reviewedAt: "2026-08-10",
    status: "approved",
  },
  {
    id: "CL-P01",
    copy: "TxProof proves correctness.",
    scope: "Universal guarantee",
    sourceIds: ["B01"],
    reviewedAt: "2026-08-10",
    status: "prohibited",
  },
];

const prohibitedPhrases = [
  "proves correctness",
  "perfect deterministic replay",
  "exactly-once payments",
] as const;

export function findProhibitedPhrase(copy: string) {
  const normalized = copy.toLocaleLowerCase("en");
  return prohibitedPhrases.find((phrase) => normalized.includes(phrase)) ?? null;
}

export function findExpiredClaims(records: ClaimRecord[], now = new Date()) {
  return records.filter(
    (claim) => claim.expiresAt && new Date(`${claim.expiresAt}T23:59:59Z`) < now,
  );
}
