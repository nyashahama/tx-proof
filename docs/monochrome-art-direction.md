# TxProof Monochrome Art Direction

Status: implementation contract for the direction following `feat/world-class-marketing-site`.

## Objective

Make TxProof feel like a serious correctness instrument: calm enough to trust, precise enough to inspect, and unmistakably about executable evidence. The visual system should share the restraint of Polar, Notion, and Linear without recreating any brand's composition, assets, components, copy, or motion.

The retained product story is strong. The visual mismatch is Direction A's ivory, vermilion, serif-editorial identity: it reads as a polished campaign and overlaps too closely with Packwork. Direction B must instead read as a forensic proof product.

## Reference findings

### Polar

- A short commercial claim and one decisive call to action create immediate focus.
- Near-black product surfaces, compact operational labels, and real interface artifacts carry credibility.
- Consistent bounded sections and restrained motion make dense financial infrastructure approachable.

Reference: [polar.sh](https://polar.sh/) and the Apache-2.0 [Polar repository](https://github.com/polarsource/polar). Structural study only.

### Linear

- The product interface is the hero rather than an illustration of the product.
- Thin borders, low-saturation surfaces, compact labels, and disciplined type establish technical authority.
- The surrounding page remains quiet so high-density product details reward close reading.

Reference: [linear.app](https://linear.app/). Visual study only.

### Notion

- White space and a simple black-on-white hierarchy give major claims room to land.
- Large product frames interrupt the page only when they add concrete understanding.
- The palette remains neutral, with color reserved for small semantic moments.

Reference: [notion.com/product](https://www.notion.com/product). Visual study only.

## Original direction: forensic monochrome

TxProof's signature object is a **Counterexample Receipt**: a bounded record that combines an invariant, witness ID, causal events, a single divergence, reproducibility, and the process exit. It should feel like a financial ledger crossed with a deterministic test report—not a generic terminal, dashboard, or developer-tool gradient.

Proof geometry supplies the visual character:

- hairline rules and explicit execution boundaries;
- trace rows with event numbers, owners, and durable state;
- witness IDs, checksums, checkpoints, and replay counts;
- light page fields interrupted by decisive near-black evidence artifacts;
- one semantic failure red, never used as brand decoration.

## Visual-system contract

### Palette

| Token | Value | Purpose |
| --- | --- | --- |
| paper | `#f8f8f6` | primary page field |
| paper bright | `#ffffff` | raised light surfaces |
| paper deep | `#eeeeeb` | quiet control fields |
| ink | `#101112` | primary text and controls |
| ink raised | `#171819` | dark evidence panels |
| muted | `#62666d` | supporting text |
| rule | `#d9dadc` | light-surface boundaries |
| inverse | `#08090a` | flagship evidence field |
| inverse panel | `#111213` | nested evidence surface |
| inverse rule | `#2a2e33` | dark-surface boundaries |
| failure | `#b42318` | failed witness and `EXIT 10` only |
| focus | `#1266d6` | keyboard focus only |

Success should normally be communicated with text, shape, and contrast. Green is not a brand color and should not be necessary to understand the state.

### Typography

- Use the existing Manrope family as the sole display/body sans to avoid a new dependency and remove the editorial serif identity.
- Keep IBM Plex Mono for commands, identifiers, timestamps, SQL, hashes, and verdicts.
- Desktop hero: `clamp(4rem, 7.2vw, 7.25rem)`, tight line-height and tracking.
- Mobile hero: `clamp(2.8rem, 13vw, 4.25rem)` with deliberate line breaks determined by available width, not hard-coded `<br>` elements.
- Body text: 16–19px, line-height 1.55–1.7. Labels remain compact but no smaller than 10px.

### Shape and material

- Primary product frame: 12–16px radius.
- Small artifacts and controls: 6–8px radius.
- Pills only for actions and status chips.
- Light surfaces are flat. Product frames use a one-pixel rule and at most `0 24px 80px rgb(0 0 0 / 8%)`.
- Dark evidence panels use borders and tonal separation, not colored glows.

### Navigation

- White, quiet, and bounded by a hairline rule.
- Preserve Product, Method, Safety, and Research as the core destinations.
- Retain the counterexample CTA, rendered as a compact black pill.
- Mobile keeps the existing accessible disclosure behavior and a clear primary path.

### Motion

- No marquees, parallax, looping simulations, background particles, or cursor theatre.
- Hover and state changes may use 150–200ms opacity, color, or transform transitions.
- The trace remains user-controlled and shows its final comprehensible state under reduced motion.

## Page composition

### `/`

- Left: direct claim, concise explanation, primary and quiet actions.
- Right: the Counterexample Receipt, derived from the existing trace instrument.
- On mobile, claim and CTA precede the artifact; the verdict remains visible without horizontal scrolling.
- Follow with a compact evidence ledger and a clear narrative: contradiction, method, controlled boundaries, owned evidence, safety, honest scope, audit.
- Remove decorative colored glow and make section changes deliberate white/black proof transitions.

### `/product`

Render the CLI, fixture, provider behavior, and PostgreSQL oracle as one bounded system frame with nested rule-based layers rather than unrelated feature cards.

### `/method`

Use a numbered vertical execution lane. Each stage reads as a deterministic event record with implementation evidence, not a marketing stepper.

### `/safety`

Use white ground and hard rules. The dark mutation-lock artifact is the centerpiece. Failure red is reserved for rejected or unsafe targets.

### `/research`

Treat claims as a source ledger: flat rows, identifiers, dates, scope notes, and links. Dark reversal is only for a claim-boundary warning.

### `/counterexamples/commit-then-close`

This is the visual flagship: a wide black witness console with one causal divergence, followed by white explanation fields for the oracle, shrink, and repair.

### `/audit`

Use an audit scope receipt and a vertical engagement flow. The page should feel like a productized evidence engagement, not generic consultancy.

## Anti-copy boundary

- Do not use Linear's violet accent, star field, screenshot composition, issue vocabulary, or phrasing.
- Do not use Polar's usage-billing visuals, blue action color, dashboard arrangement, or headline formula.
- Do not use Notion's illustrations, collage language, playful wordmark rhythm, or navigation composition.
- Do not import reference components, CSS, screenshots, testimonials, logos, icons, or animations.
- Keep TxProof's existing product claims, trace model, and original repository-native artifacts.

## Acceptance criteria

1. The homepage reads as black/white at a glance; failure red appears only where failure semantics demand it.
2. The hero contains a real, inspectable proof artifact and no decorative product mock-up.
3. The heading and body are sans; mono is restricted to machine evidence.
4. Primary CTAs are monochrome pills with visible hover and keyboard focus states.
5. Every route inherits the same light field, dark evidence surface, border, type, and spacing system.
6. The 390px layout has no horizontal overflow; code, tables, and event records wrap or recompose.
7. The trace remains keyboard-operable, screen-reader legible, and understandable without color or motion.
8. Automated accessibility has no serious or critical violations.
9. All existing behavior, metadata, crawler, and social-image contracts remain green.
10. Final review includes rendered 1440px and 390px screenshots for every route, plus reduced-motion inspection of the homepage trace.
