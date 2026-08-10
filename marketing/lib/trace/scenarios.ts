export const truthPlanes = [
  {
    id: "intent",
    shortLabel: "01 / INTENT",
    label: "Customer intent",
    description: "One checkout. One operation identity.",
  },
  {
    id: "provider",
    shortLabel: "02 / PROVIDER",
    label: "Stripe state",
    description: "The PaymentIntent is committed remotely.",
  },
  {
    id: "database",
    shortLabel: "03 / DATABASE",
    label: "PostgreSQL state",
    description: "The application records its local truth.",
  },
  {
    id: "effect",
    shortLabel: "04 / EFFECT",
    label: "Business effect",
    description: "Value is fulfilled exactly once—or is not.",
  },
] as const;

export type TruthPlane = (typeof truthPlanes)[number]["id"];
export type ScenarioId =
  | "lost-response"
  | "duplicate-webhook"
  | "reordered-event"
  | "process-crash";
export type EventTone = "quiet" | "active" | "violation" | "repaired";

export type TraceEvent = {
  id: string;
  sequence: number;
  plane: TruthPlane;
  time: string;
  label: string;
  detail: string;
  tone: EventTone;
};

export type MinimizedAction = {
  sequence: number;
  label: string;
  detail: string;
};

export type TraceScenario = {
  id: ScenarioId;
  tabLabel: string;
  title: string;
  summary: string;
  invariant: string;
  witness: string;
  reproduction: string;
  checkpoint: string;
  events: readonly TraceEvent[];
  minimized: readonly MinimizedAction[];
};

const lostResponse: TraceScenario = {
  id: "lost-response",
  tabLabel: "Lost response",
  title: "Committed remotely. Unknown locally.",
  summary:
    "Stripe commits the PaymentIntent, but the response disappears. A later retry and repeated success event split the four truths.",
  invariant: "webhook-effect-at-most-once",
  witness: "event evt_tiv_019 created 2 fulfilment rows",
  reproduction: "3/3 fresh baselines",
  checkpoint: "quiescence.after_webhook_retry",
  events: [
    {
      id: "lr-01",
      sequence: 1,
      plane: "intent",
      time: "00.000",
      label: "Checkout requested",
      detail: "POST /api/checkout · operation op_41",
      tone: "active",
    },
    {
      id: "lr-02",
      sequence: 2,
      plane: "provider",
      time: "00.042",
      label: "PaymentIntent committed",
      detail: "pi_tiv_07 · succeeded · 12,900 ZAR",
      tone: "active",
    },
    {
      id: "lr-03",
      sequence: 3,
      plane: "intent",
      time: "00.043",
      label: "Response connection closed",
      detail: "No response byte reaches the caller",
      tone: "violation",
    },
    {
      id: "lr-04",
      sequence: 4,
      plane: "provider",
      time: "00.081",
      label: "Success event delivered",
      detail: "payment_intent.succeeded · evt_tiv_019",
      tone: "active",
    },
    {
      id: "lr-05",
      sequence: 5,
      plane: "database",
      time: "00.096",
      label: "Payment marked paid",
      detail: "payments.status → paid",
      tone: "active",
    },
    {
      id: "lr-06",
      sequence: 6,
      plane: "effect",
      time: "00.101",
      label: "Order fulfilled",
      detail: "fulfilment row f_901 inserted",
      tone: "active",
    },
    {
      id: "lr-07",
      sequence: 7,
      plane: "database",
      time: "00.108",
      label: "API process killed",
      detail: "After response observed; before acknowledgement",
      tone: "violation",
    },
    {
      id: "lr-08",
      sequence: 8,
      plane: "provider",
      time: "00.344",
      label: "Event retried",
      detail: "Same event ID · second delivery attempt",
      tone: "violation",
    },
    {
      id: "lr-09",
      sequence: 9,
      plane: "effect",
      time: "00.359",
      label: "Fulfilment duplicated",
      detail: "fulfilment row f_902 inserted",
      tone: "violation",
    },
  ],
  minimized: [
    {
      sequence: 1,
      label: "Checkout requested",
      detail: "POST /api/checkout · operation op_41",
    },
    {
      sequence: 2,
      label: "PaymentIntent committed; response closed",
      detail: "pi_tiv_07 is durable; caller receives no result",
    },
    {
      sequence: 3,
      label: "Success event delivered",
      detail: "evt_tiv_019 reaches the webhook handler",
    },
    {
      sequence: 4,
      label: "API process killed",
      detail: "After handler response; before acknowledgement",
    },
    {
      sequence: 5,
      label: "Event retried",
      detail: "The same event creates a second fulfilment",
    },
  ],
};

const duplicateWebhook: TraceScenario = {
  id: "duplicate-webhook",
  tabLabel: "Duplicate webhook",
  title: "One event. Two durable effects.",
  summary:
    "The same signed event is delivered twice. Both handlers return success, and both insert a fulfilment.",
  invariant: "webhook-effect-at-most-once",
  witness: "event evt_tiv_024 mapped to effects f_311 and f_312",
  reproduction: "3/3 fresh baselines",
  checkpoint: "quiescence.after_duplicate_delivery",
  events: [
    {
      id: "dw-01",
      sequence: 1,
      plane: "intent",
      time: "00.000",
      label: "Checkout requested",
      detail: "operation op_58 · one customer action",
      tone: "active",
    },
    {
      id: "dw-02",
      sequence: 2,
      plane: "provider",
      time: "00.038",
      label: "PaymentIntent succeeded",
      detail: "pi_tiv_11 · one provider object",
      tone: "active",
    },
    {
      id: "dw-03",
      sequence: 3,
      plane: "provider",
      time: "00.071",
      label: "Event attempt 01",
      detail: "evt_tiv_024 · HTTP 200",
      tone: "active",
    },
    {
      id: "dw-04",
      sequence: 4,
      plane: "database",
      time: "00.079",
      label: "Delivery persisted",
      detail: "webhook_events row w_101",
      tone: "active",
    },
    {
      id: "dw-05",
      sequence: 5,
      plane: "effect",
      time: "00.084",
      label: "First fulfilment",
      detail: "effect f_311 committed",
      tone: "active",
    },
    {
      id: "dw-06",
      sequence: 6,
      plane: "provider",
      time: "00.311",
      label: "Event attempt 02",
      detail: "evt_tiv_024 · same raw payload",
      tone: "violation",
    },
    {
      id: "dw-07",
      sequence: 7,
      plane: "database",
      time: "00.321",
      label: "Duplicate accepted",
      detail: "No unique event guard",
      tone: "violation",
    },
    {
      id: "dw-08",
      sequence: 8,
      plane: "effect",
      time: "00.326",
      label: "Second fulfilment",
      detail: "effect f_312 committed",
      tone: "violation",
    },
  ],
  minimized: [
    { sequence: 1, label: "Checkout requested", detail: "operation op_58" },
    { sequence: 2, label: "PaymentIntent succeeded", detail: "pi_tiv_11" },
    { sequence: 3, label: "Event delivered", detail: "evt_tiv_024 · attempt 01" },
    { sequence: 4, label: "Event redelivered", detail: "evt_tiv_024 · attempt 02" },
    { sequence: 5, label: "Effect duplicated", detail: "f_311 + f_312" },
  ],
};

const reorderedEvent: TraceScenario = {
  id: "reordered-event",
  tabLabel: "Reordered event",
  title: "Success arrives before the older state.",
  summary:
    "A succeeded event is handled first. An older processing event arrives later and regresses the terminal local state.",
  invariant: "terminal-state-monotonic",
  witness: "payment pay_203 regressed succeeded → processing",
  reproduction: "3/3 fresh baselines",
  checkpoint: "quiescence.after_event_permutation",
  events: [
    {
      id: "re-01",
      sequence: 1,
      plane: "intent",
      time: "00.000",
      label: "Payment confirmed",
      detail: "operation op_63",
      tone: "active",
    },
    {
      id: "re-02",
      sequence: 2,
      plane: "provider",
      time: "00.029",
      label: "Provider reaches succeeded",
      detail: "pi_tiv_15 · terminal provider state",
      tone: "active",
    },
    {
      id: "re-03",
      sequence: 3,
      plane: "provider",
      time: "00.068",
      label: "Succeeded event delivered",
      detail: "evt_tiv_031 delivered first",
      tone: "active",
    },
    {
      id: "re-04",
      sequence: 4,
      plane: "database",
      time: "00.076",
      label: "Local state succeeds",
      detail: "payments.status → succeeded",
      tone: "active",
    },
    {
      id: "re-05",
      sequence: 5,
      plane: "effect",
      time: "00.083",
      label: "Entitlement granted",
      detail: "entitlement e_207 active",
      tone: "active",
    },
    {
      id: "re-06",
      sequence: 6,
      plane: "provider",
      time: "00.291",
      label: "Older event delivered",
      detail: "payment_intent.processing · evt_tiv_030",
      tone: "violation",
    },
    {
      id: "re-07",
      sequence: 7,
      plane: "database",
      time: "00.301",
      label: "Terminal state regressed",
      detail: "succeeded → processing",
      tone: "violation",
    },
    {
      id: "re-08",
      sequence: 8,
      plane: "effect",
      time: "00.306",
      label: "Truths now disagree",
      detail: "Effect is active; local payment is not terminal",
      tone: "violation",
    },
  ],
  minimized: [
    { sequence: 1, label: "Payment confirmed", detail: "operation op_63" },
    { sequence: 2, label: "Provider succeeds", detail: "pi_tiv_15" },
    { sequence: 3, label: "Succeeded event applied", detail: "evt_tiv_031" },
    { sequence: 4, label: "Older event applied", detail: "evt_tiv_030" },
    { sequence: 5, label: "State regressed", detail: "succeeded → processing" },
  ],
};

const processCrash: TraceScenario = {
  id: "process-crash",
  tabLabel: "Process crash",
  title: "The row commits. The acknowledgement does not.",
  summary:
    "The API dies after the database commit but before its caller observes success. A retry creates a second local payment relation.",
  invariant: "provider-object-unique",
  witness: "operation op_77 mapped to payments pay_410 and pay_411",
  reproduction: "2/3 fresh baselines",
  checkpoint: "quiescence.after_client_retry",
  events: [
    {
      id: "pc-01",
      sequence: 1,
      plane: "intent",
      time: "00.000",
      label: "Checkout requested",
      detail: "operation op_77",
      tone: "active",
    },
    {
      id: "pc-02",
      sequence: 2,
      plane: "provider",
      time: "00.035",
      label: "PaymentIntent created",
      detail: "pi_tiv_20",
      tone: "active",
    },
    {
      id: "pc-03",
      sequence: 3,
      plane: "database",
      time: "00.052",
      label: "Payment row committed",
      detail: "payment pay_410",
      tone: "active",
    },
    {
      id: "pc-04",
      sequence: 4,
      plane: "database",
      time: "00.053",
      label: "Process exits",
      detail: "SIGKILL before response acknowledgement",
      tone: "violation",
    },
    {
      id: "pc-05",
      sequence: 5,
      plane: "intent",
      time: "00.301",
      label: "Caller retries",
      detail: "Same operation; changed retry key",
      tone: "violation",
    },
    {
      id: "pc-06",
      sequence: 6,
      plane: "provider",
      time: "00.333",
      label: "Second object created",
      detail: "pi_tiv_21",
      tone: "violation",
    },
    {
      id: "pc-07",
      sequence: 7,
      plane: "database",
      time: "00.347",
      label: "Second relation committed",
      detail: "payment pay_411",
      tone: "violation",
    },
    {
      id: "pc-08",
      sequence: 8,
      plane: "effect",
      time: "00.351",
      label: "Operation identity split",
      detail: "One customer intent now has two payments",
      tone: "violation",
    },
  ],
  minimized: [
    { sequence: 1, label: "Checkout requested", detail: "operation op_77" },
    { sequence: 2, label: "Provider object created", detail: "pi_tiv_20" },
    { sequence: 3, label: "Local commit", detail: "payment pay_410" },
    { sequence: 4, label: "Process killed", detail: "before acknowledgement" },
    { sequence: 5, label: "Caller retries", detail: "second payment relation" },
  ],
};

export const scenarios: readonly TraceScenario[] = [
  lostResponse,
  duplicateWebhook,
  reorderedEvent,
  processCrash,
];

export function getScenario(id: string): TraceScenario {
  const scenario = scenarios.find((candidate) => candidate.id === id);

  if (!scenario) {
    throw new Error(`Unknown trace scenario: ${id}`);
  }

  return scenario;
}

export function minimizeScenario(
  scenario: TraceScenario,
): readonly MinimizedAction[] {
  return scenario.minimized.map((action, index) => ({
    ...action,
    sequence: index + 1,
  }));
}
