# TxProof marketing site

Research-backed Next.js marketing surface for TxProof, a bounded counterexample-search product for money-moving backends.

This checkpoint preserves the first visual direction: an editorial evidence system built around a real modeled commit-then-close failure. Passing language is intentionally bounded: no violation found under the configured model and budget is not proof of correctness.

## Routes

- `/` — product thesis and interactive failure trace
- `/product` — controlled-system, invariant, and artifact contract
- `/method` — compile, execute, check, replay, and shrink method
- `/safety` — local-only destructive-testing boundary
- `/research` — dated source ledger and claim controls
- `/counterexamples/commit-then-close` — canonical minimized witness
- `/audit` — fixed-scope Money Correctness Audit qualification

## Local development

Requirements: Node.js 20.9 or newer and pnpm 11.1.3.

```bash
pnpm install --frozen-lockfile
pnpm dev
```

Open [http://localhost:3000](http://localhost:3000).

Set `NEXT_PUBLIC_SITE_URL` to the confirmed deployment origin before release. Copy `.env.example` as the starting contract; the local fallback is `http://localhost:3000` so no unverified production domain is published from source.

## Verification

```bash
pnpm verify
```

The gate runs ESLint, TypeScript, Vitest, a production Next.js build, and Playwright against Chromium, Firefox, and WebKit. Browser checks cover interaction, primary-route contracts, 390px reflow, serious/critical Axe findings, canonical and social metadata, generated Open Graph images, crawler endpoints, and the custom 404 state.

Asset and reference provenance is recorded in [`public/provenance.json`](public/provenance.json). Claims and review dates live in [`content/claims.ts`](content/claims.ts).
