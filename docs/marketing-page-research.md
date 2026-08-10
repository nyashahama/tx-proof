# TxProof marketing-page reference research

Research date: 2026-08-10

## Decision

Build a TxProof marketing page on Polar's UI/UX foundation:

- Preserve Polar's dark canvas, calm financial presentation, spacing, panel system, section rhythm, and responsive behavior.
- Replace Polar's product content with TxProof's counterexample, safety, evidence, and audit story.
- Retain TxProof's semantic failure red only where it communicates an invariant violation.

Polar is the local source reference because its current landing page is present in the public repository under Apache-2.0. The other strongest live pages remain visual references only where their equivalent marketing source is not publicly available.

Reference checkout:

- Repository: <https://github.com/polarsource/polar>
- Local path: `reference-projects/polar`
- Commit: `4328fac5b6951ca8e90b7d318cdb39bf31e88bdd`
- Sparse paths: `clients/apps/web`, `clients/packages`
- License: Apache-2.0

The license permits adapting the implementation under its terms. It does not grant rights to Polar's name, marks, customer logos, testimonials, photography, copy, or product screenshots. None of those assets should enter TxProof.

## Evidence matrix

| Reference | Strongest lesson | Source decision |
| --- | --- | --- |
| [Polar](https://polar.sh/) / [GitHub](https://github.com/polarsource/polar) | Calm financial authority, composable sections, bespoke geometric graphics, and a clear usage-to-revenue flow | Primary local clone; Apache-2.0 and source-complete |
| [Infisical](https://infisical.com/) / [GitHub](https://github.com/Infisical/infisical) | Security-critical infrastructure can feel editorial and visually arresting without losing clarity | Visual reference only; current homepage source was not confirmed in the public repository |
| [Inngest](https://www.inngest.com/) / [GitHub](https://github.com/inngest/inngest) | Strong hero-to-code-to-adoption-to-security page rhythm | Visual reference only; public repository contains product UI rather than the live marketing application |
| [Supabase](https://supabase.com/) / [GitHub](https://github.com/supabase/supabase) | Database-native credibility, code/product alternation, and explicit open-source proof | Permissive fallback, but a much heavier checkout and an over-copied visual language |
| [SigNoz](https://signoz.io/) / [GitHub](https://github.com/SigNoz/signoz) | Turns checkout behavior into traces, evidence, and an actionable diagnosis | Evidence-presentation reference; avoid its dense carousel and logo volume |
| [Trigger.dev](https://trigger.dev/) / [GitHub](https://github.com/triggerdotdev/trigger.dev) | Real code and execution state make an infrastructure promise tangible | Visual reference only; the polished marketing homepage was not located in the public app source |
| [OpenStatus](https://www.openstatus.dev/) / [GitHub](https://github.com/openstatusHQ/openstatus) | Lightweight, source-complete Next.js marketing implementation | Not selected because AGPL-3.0 is a poor code-copy base for an unconstrained product site |

## What Polar's source teaches us

The landing page is deliberately decomposed rather than authored as one large marketing component:

- Route: `clients/apps/web/src/app/(main)/(website)/(landing)/page.tsx`
- Composition: `clients/apps/web/src/components/Landing/LandingPage.tsx`
- Hero: `clients/apps/web/src/components/Landing/Hero/Hero.tsx`
- Feature tiles: `clients/apps/web/src/components/Landing/Features.tsx`
- Three-stage explanation: `clients/apps/web/src/components/Landing/Usage.tsx`
- Financial flow: `clients/apps/web/src/components/Landing/Pipeline.tsx`
- Scroll narrative: `clients/apps/web/src/components/Landing/Vision.tsx`
- Bespoke graphics: `clients/apps/web/src/components/Landing/graphics/`

The transferable patterns are:

1. One product idea per section.
2. Bespoke graphics encode the product rather than decorating empty space.
3. A restrained palette lets evidence and typography carry authority.
4. Dense explanations are broken into a visual system, short copy, and progressive disclosure.
5. Motion is concentrated in a few narrative moments instead of applied to every card.
6. The final conversion section follows a complete technical explanation rather than interrupting it.

## TxProof visual thesis

TxProof should feel like Polar's calm, dark financial product experience applied to an adversarial correctness tool: precise, spacious, restrained, and trustworthy. It should not resemble a generic AI platform, cyberpunk security page, or gradient-heavy SaaS template.

The signature visual is the four-truth schedule:

```text
Customer intent     request ───────── retry ──────────────────────────
Stripe state                 commit ─────────────── pi_succeeded ─────
PostgreSQL state                    pending ─ paid ───── paid again ──
Business effect                            fulfil ───── fulfil again ─
                                                        ▲
                                              invariant violation
```

The user should be able to change one fault—lost response, duplicate webhook, reordered event, or process crash—and watch the four truth planes diverge. TxProof then shrinks the noisy history into a stable five-step counterexample.

This is the spectacle. It is also the product explanation.

### Material and tone

- One continuous near-black canvas, light type, muted gray copy, and slightly raised charcoal panels.
- One contradiction accent in vermilion or signal orange.
- One restrained proof/recovery accent used only when an invariant holds again.
- Large editorial grotesk typography paired with a highly legible mono for traces and SQL evidence.
- Fine ledger rules, checkpoint marks, hashes, and event IDs as functional texture.
- No generic glowing orb, stock dashboard, fake terminal, or decorative blockchain imagery.

### Trust rules

- Never say that TxProof proves correctness or guarantees exactly-once payments.
- Do not invent customer logos, testimonials, benchmark numbers, certifications, or usage counts.
- Put “counterexample search, not proof” in the primary product explanation.
- Make local-only execution, disposable-database guards, redaction, and explicit SQL invariants visible product features.
- Show a real modeled trace and honest limitations rather than an aspirational dashboard.

## Proposed homepage anatomy

### 1. Navigation

Product, How it works, Safety, Research, and a single primary action: `Book a correctness audit`.

### 2. Hero

Eyebrow: `Counterexample search for money-moving backends`

Working headline: `Find the schedule that makes your database lie about money.`

Support the claim with Stripe, PostgreSQL, webhook, retry, and crash language. Use two actions: `View a failing trace` and `Book a correctness audit`.

The hero should stay concise. It resolves into three product-specific capability visuals—causal modeling, SQL interrogation, and validity-aware shrinking—before the full interactive trace. On mobile, the product story must precede the dense trace rather than forcing the workbench into the opening viewport.

### 3. The four truths

Explain customer intent, provider state, application state, and business effect. Demonstrate that no individual component has to be broken for the system to become contradictory.

### 4. Three-step product mechanism

1. Declare five repository-approved SQL invariants.
2. Search valid payment, retry, webhook, and crash schedules.
3. Shrink a reproducible failure into a normal CI regression.

### 5. Counterexample workbench

Show one complete reference failure:

- Stripe commits a PaymentIntent.
- The response connection closes.
- The caller retries with the wrong key.
- A webhook is delivered twice around a process restart.
- The database produces a two-row invariant witness.

The UI should move from full history to minimized trace to passing replay after a fix.

### 6. Failure-model tiles

Provider ambiguity, webhook delivery, client retry, process lifecycle, and reconciliation timing. Each tile needs a concrete invariant risk rather than a generic feature label.

### 7. Safety boundary

Explain the exact database identity guard, live-key rejection, public-IP denial, local artifacts, redaction, and “exit before mutation” behavior. This section replaces unsupported enterprise trust badges.

### 8. Honest comparison boundary

Clarify that TxProof is not a Stripe clone, production chaos platform, formal proof system, ledger, or observability product. Link the differentiation to actual PostgreSQL invariants and minimized regressions.

### 9. Audit conversion

The current commercial action is a fixed-scope Money Correctness Audit, not a fictional self-serve SaaS signup. Explain the qualifying stack and expected artifact before asking for contact.

## Interaction principles

- Motion must explain causal order, checkpointing, divergence, or shrinking.
- The page remains fully understandable with JavaScript animation disabled.
- `prefers-reduced-motion` replaces scroll-linked animation with discrete stable states.
- Mobile reflows the four tracks into one chronological event ledger; it does not squeeze a desktop diagram.
- Keyboard users can select fault scenarios and inspect evidence without hover.
- Decorative event noise stays hidden from assistive technology; the semantic trace remains concise.

## Initial acceptance criteria

- Original TxProof identity; no copied brand assets or copy.
- Visually coherent at 1440 px, 1024 px, 768 px, and 390 px widths.
- A real counterexample artifact is reached through the first product sequence: hero, three capability visuals, inspectable evidence strip, then the full interactive trace.
- No false social proof or unsupported security claim.
- No serious or critical automated accessibility findings.
- Complete keyboard navigation and visible focus treatment.
- Reduced-motion mode preserves every explanation and action.
- Stable layout with no horizontal overflow at 390 px.
- Performance budget defined before adding video, WebGL, or large raster assets.
- Each animation and dependency must have a demonstrated narrative purpose.

## Implementation sequence

1. Confirm the visual thesis and homepage anatomy.
2. Establish the marketing application, typography, tokens, and responsive shell.
3. Build the static hero and four-truth diagram first.
4. Add the counterexample interaction using deterministic fixture data.
5. Implement the remaining sections from the execution blueprint.
6. Run browser review at every target viewport, accessibility checks, and performance measurement.
7. Perform an independent copy/claims/source-provenance review before publication.

The execution blueprint remains the product and claims authority: `../transactional_invariant_verifier_complete_execution_blueprint_2026-08-10.docx`.
