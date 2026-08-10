# TxProof world-class marketing site execution plan

Status: implemented direction — Polar-aligned dark marketing experience

Date: 2026-08-10

## 1. Objective

Build a premium Next.js marketing site that makes TxProof understandable, credible, and memorable before the product engine is complete, using the pinned Polar landing experience as the UI/UX baseline and replacing its product story with TxProof content.

The site must preserve Polar's dark visual and interaction grammar while becoming a TxProof marketing experience through:

1. More product-specific above the fold.
2. More technically demonstrative instead of decorative.
3. More honest about its model, safety boundary, and claims.
4. More deliberate on mobile rather than merely responsive.
5. More rigorously verified for accessibility, performance, and browser behavior.

“Better” will not mean more animation, more gradients, or a longer page. It means a visitor can understand the problem, see the mechanism, trust the boundaries, and choose the next action with less ambiguity.

## 2. Quality scorecard

Every release candidate is scored out of 100. It cannot ship below 90, and no hard gate may fail.

| Dimension | Weight | Evidence |
| --- | ---: | --- |
| Product clarity and copy | 20 | Five-second comprehension review, claims audit, route-specific message hierarchy |
| Polar UI/UX fidelity | 20 | Side-by-side desktop/mobile screenshots, dark-surface, typography, spacing, panel, and interaction audit |
| Product-specific interaction | 20 | Four-truth schedule and counterexample workbench explain real TxProof behavior |
| Trust and evidence | 15 | Safety boundary, source register, honest limitations, no invented social proof |
| Responsive accessibility | 15 | WCAG 2.2 AA, keyboard, reduced motion, 390 px reflow, Axe and manual checks |
| Performance and engineering | 10 | Production build, bundle report, Lighthouse, Core Web Vitals budgets, clean console |

Hard failures:

- Unsupported correctness, security, customer, benchmark, or competitive claim.
- No meaningful experience without animation.
- Serious or critical accessibility issue.
- Horizontal overflow at 390 px.
- A broken route, console error, hydration error, or failed production build.
- Unlicensed or unattributed copied asset.

## 3. Product and claims authority

The sources of truth, in order, are:

1. `transactional_invariant_verifier_complete_execution_blueprint_2026-08-10.docx`
2. `docs/marketing-page-research.md`
3. Current primary vendor and standards documentation.
4. The pinned Polar reference at `reference-projects/polar`.

The reference is an Apache-2.0 source and UI/UX baseline. TxProof may adapt Polar's layout, spacing, dark materials, panel geometry, responsive patterns, and interaction grammar. TxProof will not reuse Polar's name, marks, marketing copy, customer proof, photography, or product screenshots.

## 4. Technical architecture

### Application boundary

Create a standalone Next.js application at `marketing/`. It remains independent of the future Rust workspace and can be built, tested, previewed, and deployed without the verifier engine.

### Framework decisions

- Next.js App Router, current stable version at scaffold time, pinned in the lockfile.
- TypeScript strict mode with no implicit `any` escape hatches.
- Server Components by default.
- Client Components only for navigation state, the four-truth schedule, and the counterexample workbench.
- Static rendering for all marketing and research content unless a concrete dynamic requirement appears.
- CSS Modules plus a small global CSS-token layer for bespoke styling and minimal runtime cost.
- `next/font` with locally hosted, license-recorded variable fonts.
- Semantic HTML and original inline SVG for the product diagrams.
- Motion as the only animation library if the static/CSS prototype proves it is needed; load interactive motion code only with the relevant island.
- No GSAP, WebGL, background video, design-system framework, CMS, analytics SDK, or form vendor without a measured need.

### Proposed structure

```text
marketing/
├── app/
│   ├── (marketing)/
│   │   ├── page.tsx
│   │   ├── product/page.tsx
│   │   ├── method/page.tsx
│   │   ├── safety/page.tsx
│   │   ├── research/page.tsx
│   │   ├── audit/page.tsx
│   │   └── counterexamples/commit-then-close/page.tsx
│   ├── layout.tsx
│   ├── sitemap.ts
│   ├── robots.ts
│   └── not-found.tsx
├── components/
│   ├── shell/
│   ├── home/
│   ├── product/
│   ├── evidence/
│   └── interactive/
├── content/
│   ├── claims.ts
│   ├── sources.ts
│   └── counterexamples.ts
├── lib/trace/
│   ├── model.ts
│   ├── scenarios.ts
│   └── reducer.ts
├── styles/
│   ├── tokens.css
│   ├── globals.css
│   └── motion.css
├── public/
│   └── provenance.json
└── tests/
    ├── unit/
    └── e2e/
```

## 5. Brand and design system

### Character

TxProof should feel like a forensic financial instrument: calm, exact, adversarial, and expensive in the sense of craft—not luxurious decoration.

### Visual material

- A continuous near-black canvas with light type and raised charcoal panels, matching Polar's dark presentation.
- Vermilion or signal orange only for contradiction, failure, and causal emphasis.
- A restrained mineral green only for repaired or holding invariants.
- Fine ledger rules, checkpoint ticks, hashes, event IDs, and SQL witnesses as functional texture.
- Large editorial typography for claims and highly legible mono for evidence.
- Custom diagrams derived from actual product semantics, never generic technology art.

### Token system

Define tokens before components:

- Surface, text, border, contradiction, proof, and muted colors.
- A four-step type scale for body/evidence and a fluid display scale using `clamp()`.
- An 8 px spatial foundation with named section, panel, and control spacing.
- Border, radius, shadow, line-weight, motion-duration, and easing tokens.
- Breakpoints chosen from content failure points, then normalized around 390, 768, 1024, and 1440 px validation widths.

### Typography selection gate

Audition three license-safe pairings in the real hero, not in an alphabet specimen. Score legibility, numerical clarity, punctuation, mono alignment, weight range, and total WOFF2 cost. Record the license and source in `public/provenance.json` before adoption.

### Asset contract

- Prefer original HTML, CSS, and SVG.
- Every external image or font records its source, author, license, local path, checksum, and dimensions.
- No remote runtime assets.
- No customer or company mark without written permission.
- Generated textures must remain optional and cannot carry product meaning.

## 6. Site map and the job of each route

| Route | Visitor question | Primary conversion | Signature visual |
| --- | --- | --- | --- |
| `/` | What is this, why should I care, and can I trust it? | View a failing trace / Book an audit | Interactive four-truth schedule |
| `/product` | What exactly does TxProof control and produce? | Inspect the artifact contract | Search engine and artifact anatomy |
| `/method` | How does counterexample search work without claiming proof? | Open canonical counterexample | Compile → execute → check → replay → shrink |
| `/safety` | Why is a destructive harness safe to run locally? | Review preflight contract | Database identity gate and mutation boundary |
| `/research` | What evidence supports the failure model and differentiation? | Review sources / benchmark method | Source ledger and claims boundary |
| `/counterexamples/commit-then-close` | Show me one real failure from start to finish | Replay the modeled trace | Full versus minimized versus fixed trace |
| `/audit` | Is my repository qualified and what happens next? | Request qualification | Audit timeline and deliverable bundle |

Legal routes are added before collecting analytics or personal information, not as empty boilerplate.

## 7. Homepage section plan

### 7.1 Navigation

Job: establish calm authority, locate the technical story, and make the commercial action obvious.

Design:

- 64–72 px shell with a compact original TxProof wordmark.
- Product, Method, Safety, Research.
- Secondary text link to the canonical counterexample.
- One filled action: `Book a correctness audit`.
- No oversized mega-menu in the first version.

Behavior:

- Transparent over the hero, then an opaque evidence-paper surface after scrolling.
- Full keyboard navigation, skip link, visible focus, Escape-close mobile menu.
- Mobile drawer uses the reading order of the site and preserves the audit action.

Proof:

- Keyboard and screen-reader navigation test.
- Screenshot at 390, 768, 1024, and 1440 px.
- No layout jump when the sticky state changes.

### 7.2 Hero — the premium standard

Job: explain TxProof and demonstrate its core mechanism in the first meaningful viewport.

Working content hierarchy:

- Eyebrow: `Counterexample search for money-moving backends`
- Headline: `Find the schedule that makes your database lie about money.`
- Support: the exact Stripe, PostgreSQL, webhook, retry, and crash boundary.
- Primary action: `View a failing trace`
- Secondary action: `Book a correctness audit`
- Honest qualifier: `Counterexample search, not proof.`

Desktop composition:

- Copy uses Polar's centered, compact hero stack with restrained width and generous dark negative space.
- The four truth planes form a full-width raised product proof beneath the hero copy.
- Customer intent, Stripe, PostgreSQL, and business effect have distinct rows connected by causal events.
- A lost response creates the first visible divergence; a later duplicate effect creates the invariant witness.
- The terminal result is integrated as a proof label, not placed inside a fake terminal window.

Interaction:

- Default state is already understandable as a still composition.
- Four scenario controls: lost response, duplicate webhook, reordered event, process crash.
- Selecting a scenario changes a deterministic typed trace; no random visual behavior.
- The first entrance may reveal causal order once, then stops. It never loops continuously.
- A `Minimize trace` action transforms the long schedule into the five decisive actions.
- Scenario changes announce one concise outcome through an accessible status region.

Mobile transformation:

- Copy first, actions second, chronological evidence ledger third.
- The four horizontal tracks become a single vertical event sequence grouped by truth plane.
- Controls use a horizontally scrollable, keyboard-operable tab list only if all labels remain visible; otherwise use a stacked selector.
- No squeezed desktop diagram and no essential hover state.

Reduced motion:

- Show discrete `before`, `violation`, and `minimized` states.
- Replace path travel and positional transitions with opacity and border emphasis.
- No auto-scrolling, parallax, or background motion.

Hero acceptance criteria:

- A first-time engineer can state what TxProof does after five seconds.
- Headline, qualifier, and one trace witness render in server HTML.
- LCP is text or lightweight SVG, never video or a large bitmap.
- No clipped headline from 320–1600 px.
- All scenarios work by keyboard and pointer.
- No-JavaScript state still explains one canonical failure.
- Reduced-motion screenshot is complete and visually intentional.
- Visual review approves all four target widths before section 7.3 begins.

### 7.3 Evidence strip without fake social proof

Job: establish immediate credibility before TxProof has customer logos.

Use verifiable product facts:

- Local-only execution.
- Stripe PaymentIntent + PostgreSQL wedge.
- Five explicit SQL invariants.
- Fresh-baseline replay classification.
- Markdown, JSON, JUnit, and checksum artifacts.

Design the strip as a signed evidence ledger with small labels and strong numerical alignment. Every statement links to a deeper route or source.

### 7.4 The four truths

Job: teach the underlying problem before listing features.

Visual:

- Four planes arranged around a central semantic operation ID.
- Each plane shows the durable state it owns.
- A single ambiguous network outcome reveals why individually valid systems can disagree.

Interaction:

- Scroll or focus reveals one plane at a time.
- Selecting a plane highlights its writes and downstream consequence.
- Static fallback presents all four with numbered relationships.

Acceptance:

- Relationships remain understandable in DOM reading order.
- No animation is required to discover the fourth plane.
- Labels use actual domain language from the blueprint.

### 7.5 Declare → Search → Shrink

Job: explain the product in three memorable actions.

Visual:

1. `Declare` shows five repository-owned SQL invariant cards.
2. `Search` shows a valid schedule compiler selecting eligible actions.
3. `Shrink` removes irrelevant events while preserving the failure identity.

Unlike generic feature cards, the three panels form one connected state transformation. On mobile, they become a numbered vertical procedure.

### 7.6 Counterexample workbench

Job: provide the deepest product proof on the homepage.

States:

- `Original`: the full generated schedule and database witness.
- `Reproduced`: 3/3 fresh-baseline classification.
- `Minimized`: five decisive actions.
- `Fixed`: identical replay with all five invariants holding.

Layout:

- Causal timeline on the left.
- Invariant SQL witness and provider/local evidence on the right.
- Bottom artifact rail shows replay command, JUnit result, trace hash, and exit code.

Engineering:

- All fixture data is typed and deterministic.
- The reducer and scenario transitions are unit-tested before animation.
- Shareable state is represented by a stable URL fragment or route, not hidden component state.

### 7.7 Failure model

Job: show bounded depth without pretending to be a generic chaos platform.

Five concrete families:

- Provider API ambiguity.
- Webhook delivery.
- Caller retry behavior.
- Process lifecycle.
- Reconciliation timing.

Each card must answer: `What changes?`, `What remains real?`, and `Which invariant is at risk?`

The visual vocabulary changes per failure family but stays inside the same evidence system. Avoid interchangeable icon cards.

### 7.8 CI artifact anatomy

Job: convert “interesting bug” into “owned engineering regression.”

Show the actual artifact tree, manifest compatibility fingerprint, original and minimized traces, JUnit file, redacted evidence, replay command, and exit codes. Use expandable code/evidence panels with copy buttons and visible focus.

### 7.9 Safety boundary

Job: make destructive behavior understandable and trustworthy.

Signature visual:

- A database identity card containing name prefix, server fingerprint, OID, owner, marker UUID, and Compose project.
- Each check locks in sequence.
- Mutation remains visibly disabled until every identity check holds.
- Live key, public IP, wrong marker, or oversized database produces an explicit pre-mutation exit.

This section links to `/safety` for the full threat model.

### 7.10 Honest comparison and claims boundary

Job: prevent misclassification and build credibility through specificity.

Use a compact matrix:

- Not a Stripe clone.
- Not production chaos.
- Not a formal proof system.
- Not an observability dashboard.
- Not a workflow runtime.

Then state the wedge: customer database, cross-system schedule, financial invariant, minimized regression.

Competitor statements live on `/research`, include a source and research date, and are never framed as permanent truth.

### 7.11 Audit conversion

Job: sell the current product honestly: a fixed-scope Money Correctness Audit.

Show:

- Qualifying stack.
- One operation and five approved invariants.
- Repository pairing and local-only boundary.
- Expected evidence bundle.
- Repair replay and CI handoff.

The form asks only for information required to qualify the audit. If no form backend is authorized, use a clear email action rather than a nonfunctional form.

### 7.12 Footer

Job: finish with utility and trust, not another sales wall.

Include product routes, research cut-off, source/claims note, security contact placeholder only when operational, legal links when real, and the sentence `Passing means no violation found under this model and budget—not correctness.`

## 8. Supporting route plans

### `/product`

Purpose: product surface without the commercial compression of the homepage.

Sections:

1. Product contract hero with one command and one minimal result.
2. Controlled system map: CLI, Compose application, Stripe fixture, PostgreSQL, artifacts.
3. Fault families and their exact variants.
4. Five-invariant model and zero-row contract.
5. Replay identity and 3/3, 2/3, 1/3 classification.
6. Artifact compatibility fingerprint.
7. Explicit v0 exclusions.

Premium mechanism: an exploded artifact diagram whose parts map directly to the CLI output.

### `/method`

Purpose: explain why the method is credible without overstating determinism.

Sections:

1. Four-truth divergence.
2. State-valid schedule compilation.
3. Observable crash cut points.
4. Quiescence and eventual invariants.
5. Fresh-baseline replay.
6. Validity-aware shrinking.
7. Determinism boundary versus hermetic simulation.

Premium mechanism: one canonical schedule remains visible while each stage annotates and transforms it.

### `/safety`

Purpose: answer the strongest adoption objection.

Sections:

1. Non-negotiable local destructive boundary.
2. Preflight decision tree.
3. Exact database identity binding.
4. Live Stripe and public-network rejection.
5. Secret and artifact redaction.
6. Resource caps and cleanup/recovery behavior.
7. Threat table with risk, mandatory control, and residual limitation.

Premium mechanism: interactive preflight simulator with safe synthetic inputs; it never connects to a real service.

### `/research`

Purpose: make trust inspectable.

Sections:

1. Research cut-off and limitations.
2. Provider-behavior source register.
3. Prior art and method lineage.
4. Competitor evidence matrix separating documented claims from benchmarks.
5. Proposed head-to-head benchmark protocol.
6. Claims blacklist.

Premium mechanism: filterable source ledger with stable source IDs used elsewhere on the site. Keep the default page fully server-rendered and indexable.

### `/counterexamples/commit-then-close`

Purpose: give technical buyers a linkable demonstration.

Sections:

1. Preconditions and declared invariant.
2. Full compiled trace.
3. Provider and local evidence at the failed checkpoint.
4. Replay results.
5. Shrink history.
6. Five-action minimized trace.
7. Corrected implementation behavior.
8. Scope and determinism caveat.

Premium mechanism: the same typed fixture powers the hero, homepage workbench, and this route, preventing contradictory demos.

### `/audit`

Purpose: qualify real buyers without pretending the SaaS exists.

Sections:

1. Audit promise and who it is for.
2. Required repository seams.
3. Eight-step engagement flow.
4. Deliverables.
5. Security and data boundary.
6. Qualification questions.
7. Contact action.

Pricing appears only when the commercial decision is confirmed; hypotheses from the blueprint are not silently published as settled pricing.

## 9. Content and claim-control workflow

Every factual or comparative claim receives:

- Stable claim ID.
- Exact display copy.
- Source URL or blueprint section.
- Research date.
- Scope/qualifier.
- Page locations.
- Status: approved, hypothesis, stale, or prohibited.

`content/claims.ts` becomes the reusable registry. A test rejects expired competitor claims and prohibited phrases such as “proves correctness,” “perfect deterministic replay,” or “exactly-once payments.”

Copy review happens before visual polish so design never hides a weak or unsupported message.

## 10. Slice-by-slice implementation method

Every major section follows the same bounded loop:

1. Re-read the blueprint and relevant reference evidence.
2. Write section-specific acceptance criteria.
3. Add failing semantic/behavior tests for the contract.
4. Implement accessible static HTML with no animation.
5. Prove mobile and desktop composition in the browser.
6. Add the smallest meaningful interaction.
7. Add reduced-motion behavior and keyboard semantics.
8. Capture deterministic screenshots at all target widths.
9. Run independent visual and code/accessibility review.
10. Repair findings, with no more than two evaluator passes.
11. Run focused tests plus the full marketing gate.
12. Record the checkpoint, screenshots, commands, and residual risk.

No section is called complete because its isolated screenshot looks good. It must also survive the full page, production build, and independent review.

## 11. Coordination model

One coordinator retains architecture, writing, integration, user communication, and the completion claim.

Use at most two independent read-only workers at a time:

- Visual reviewer: hierarchy, originality, responsive composition, interaction clarity, and comparison against the approved references.
- Engineering verifier: tests, accessibility, reduced motion, performance, console/network behavior, and source provenance.

There is only one writer in the implementation checkout. Parallel writers are prohibited unless separate worktrees and serial integration are explicitly justified.

Independent review checkpoints:

1. Static design system and hero composition.
2. Interactive hero and counterexample state model.
3. Complete homepage.
4. Supporting routes.
5. Production candidate.

## 12. User-visible checkpoints

### Checkpoint A — visual foundation

- Typography audition in the actual hero.
- Color/material sheet.
- Static 1440 and 390 hero screenshots.
- Claims hierarchy and exact CTA copy.

### Checkpoint B — signature interaction

- Four scenarios working with deterministic data.
- Keyboard and reduced-motion versions.
- Original → violated → minimized states.
- Focused tests and browser captures.

### Checkpoint C — complete homepage

- All homepage sections composed together.
- Full-page screenshots at four widths.
- Copy/claims review.
- Performance and accessibility baseline.

The second visual pass follows Polar's current pacing without copying its identity: a quiet centered hero, three tall code-native product graphics, a separately framed live product demo, one long-form editorial reset, and a high-contrast conversion close. TxProof uses only its own claims, fixtures, routes, and graphics. Every compact evidence-rail claim links to the supporting product, method, safety, counterexample, or audit route, and modeled evidence is labeled as modeled rather than customer proof.

### Checkpoint D — supporting routes

- Product, Method, Safety, Research, Counterexample, and Audit routes.
- Cross-route navigation and metadata.
- Consistent visual system without duplicated prose.

### Checkpoint E — production candidate

- Fresh full verification.
- Independent visual and engineering approval.
- Bundle and Lighthouse report.
- Exact local URL and production-like browser proof.
- Publication remains a separate authorization step.

## 13. Verification contract

The final scripts will expose commands equivalent to:

```bash
pnpm lint
pnpm typecheck
pnpm test
pnpm test:e2e
pnpm build
pnpm verify
```

### Browser matrix

- Chromium, Firefox, and WebKit for behavior.
- Deterministic Chromium screenshots at 390×844, 768×1024, 1024×768, and 1440×1000.
- Light and dark only if both themes are intentionally supported; do not ship an accidental theme toggle.
- Normal motion and reduced motion.
- JavaScript-enabled and one semantic no-JavaScript smoke pass.

Playwright screenshots are generated and compared in the same environment. Snapshot updates are reviewed as design changes, never accepted mechanically.

### Accessibility gate

- WCAG 2.2 AA target.
- Axe: no serious or critical findings.
- Complete keyboard route and interaction pass.
- Visible and unobscured focus.
- Semantic headings, regions, buttons, tabs, and status messages.
- Body text contrast at least 4.5:1; large text and graphical controls at least 3:1.
- Controls target 44×44 CSS px where practical and never below WCAG minimum.
- Reflow and text zoom tested without loss of information.
- No color-only failure/proof meaning.
- Reduced motion preserves all content and state changes.

### Performance gate

Official field targets:

- LCP ≤ 2.5 s.
- INP ≤ 200 ms.
- CLS ≤ 0.1 at the 75th percentile.

Stricter production-build lab targets:

- Median mobile Lighthouse performance ≥ 95 across three clean runs.
- Lab LCP ≤ 2.0 s.
- CLS ≤ 0.05.
- TBT ≤ 150 ms.
- Total first-load JavaScript ≤ 180 KB gzip, with route-specific interactive code ≤ 70 KB gzip.
- No above-fold video, WebGL, or unbounded raster payload.
- No third-party script before consent and measured justification.

Fonts use `next/font`; images reserve dimensions and use `next/image` where raster assets exist. Client boundaries and bundle output are reviewed before adding an animation dependency.

### SEO and trust gate

- Unique title, description, canonical URL, and original OG image per primary route.
- Sitemap and robots configuration.
- Accurate heading hierarchy and link purpose.
- Structured data only where the represented entity or service actually exists.
- No hidden text, keyword stuffing, fake review schema, or invented organization facts.
- All external links and source IDs checked from a production build.

## 14. Build order

1. Scaffold the standalone Next.js app and verification contract.
2. Implement tokens, typography, shell, and metadata foundation.
3. Build the static hero and static four-truth diagram.
4. Implement and test the deterministic trace model.
5. Add hero scenario controls, shrinking, keyboard, and reduced motion.
6. Compose the evidence strip and four-truth teaching section.
7. Build Declare → Search → Shrink and the counterexample workbench.
8. Add failure model, CI artifact, safety, claims boundary, and audit conversion.
9. Complete the homepage integration gate.
10. Build supporting routes by reusing the established evidence components.
11. Complete accessibility, performance, metadata, and asset-provenance work.
12. Run independent final review and production-like browser verification.

## 15. Definition of complete

The marketing site is complete only when:

- Every route in the initial site map exists and has route-specific value.
- The hero interaction is meaningful, deterministic, keyboard accessible, reduced-motion safe, and mobile-native.
- The page contains no unsupported claim or copied identity.
- The full verification contract passes from a clean installation.
- Browser proof exists at every required viewport.
- The production build meets the accessibility and performance gates or any exception is explicitly approved.
- Independent visual and engineering reviews have no unresolved critical or important findings.
- The local production URL and exact commands are reported before any publication request.

Implementation must not begin until this plan and the visual thesis in `docs/marketing-page-research.md` are accepted as the working contract.
