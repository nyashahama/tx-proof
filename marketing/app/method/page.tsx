import Link from "next/link";

import { createPageMetadata } from "@/lib/site";

import styles from "../technical.module.css";

export const metadata = createPageMetadata(
  "Counterexample search method",
  "How TxProof compiles state-valid schedules, replays failures, and performs validity-aware shrinking.",
  "/method",
);

const stages = [
  ["01", "Compile", "Sample only eligible next actions from the PaymentIntent state machine. Capture every dynamic value immediately."],
  ["02", "Execute", "Reset fixture and database, release actions at observable boundaries, then wait for declared quiescence."],
  ["03", "Check", "Freeze provider state and run five SQL invariants in one read-only repeatable-read snapshot."],
  ["04", "Replay", "Use the compiled trace on fresh baselines. Classify 3/3 stable, 2/3 reproducible, and 1/3 inconclusive."],
  ["05", "Shrink", "Delete or simplify actions only while the same failure identity persists within the bounded budget."],
];

export default function MethodPage() {
  return (
    <main className={styles.page}>
      <section className={`${styles.hero} ${styles.heroDark}`}>
        <div className="site-shell">
          <p className={styles.kicker}>EXECUTION SEMANTICS / BOUNDED SEARCH</p>
          <div className={styles.heroGrid}>
            <div className={styles.heroCopy}>
              <h1>Search the schedules your happy path never chooses.</h1>
              <p>TxProof generates state-valid external interleavings. It does not enumerate every permutation, control your runtime scheduler, or call a passing campaign proof.</p>
              <div className={styles.heroLinks}><Link className="button button-primary" href="/counterexamples/commit-then-close">Follow one schedule <span>↘</span></Link></div>
            </div>
            <div className={styles.scheduleCard}>
              <div className={styles.cardBar}><span>ELIGIBLE ACTIONS</span><code>state pi.created</code></div>
              <div className={styles.scheduleRows}>
                <div><span>01</span><strong>release provider commit</strong><code>chosen</code></div>
                <div><span>02</span><strong>close response connection</strong><code>eligible</code></div>
                <div><span>03</span><strong>deliver evt_tiv_019</strong><code>blocked</code></div>
                <div><span>04</span><strong>kill api container</strong><code>blocked</code></div>
              </div>
              <div className={styles.scheduleDecision}><span>RNG 424242 / 07</span><strong>→ action 01</strong></div>
            </div>
          </div>
        </div>
      </section>

      <section className="section">
        <div className="site-shell">
          <header className={styles.sectionHeading}><span>01 / ONE DECISION TASK</span><h2>Compile → execute → check → replay → shrink.</h2><p>Async completion order never consumes random numbers. The schedule is decided, captured, and made inspectable.</p></header>
          <div className={styles.stageGrid}>
            {stages.map(([index, title, description]) => <article key={title}><span>{index}</span><h3>{title}</h3><p>{description}</p></article>)}
          </div>
          <div className={styles.decisionStatement}><strong>Compiled trace—not the seed—is replay authority.</strong><p>A seed reproduces harness choices for one fixed tool and configuration. The compiled external schedule records what actually needs to happen again.</p></div>
        </div>
      </section>

      <section className={`${styles.darkSection} section`}>
        <div className="site-shell">
          <header className={`${styles.sectionHeading} ${styles.headingDark}`}><span>02 / DETERMINISM BOUNDARY</span><h2>Control what can be controlled. Name everything that cannot.</h2><p>Honesty about the boundary is part of the method—not fine print.</p></header>
          <div className={styles.controlMatrix}>
            <div><span>CONTROLLED</span><ul><li>provider outcomes</li><li>client retry releases</li><li>webhook attempts</li><li>observable process kills</li><li>fixture + database reset</li></ul></div>
            <div><span>UNCONTROLLED</span><ul><li>application threads</li><li>runtime scheduler</li><li>PostgreSQL background work</li><li>kernel timing</li><li>wall clocks and entropy</li></ul></div>
          </div>
          <p className={styles.boundaryCallout}>TxProof can produce a stable external counterexample. It cannot promise “perfect deterministic replay.”</p>
        </div>
      </section>

      <section className="section">
        <div className="site-shell">
          <header className={styles.sectionHeading}><span>03 / VALIDITY-AWARE SHRINKING</span><h2>Remove noise without changing the reason it failed.</h2><p>Every candidate starts from a fresh baseline and must preserve invariant plus checkpoint identity.</p></header>
          <ol className={styles.shrinkSteps}>
            <li><span>14</span><div><strong>Original schedule</strong><small>immutable evidence</small></div></li>
            <li><span>09</span><div><strong>Delete action chunks</strong><small>hierarchical delta debugging</small></div></li>
            <li><span>07</span><div><strong>Remove operations</strong><small>preserve state preconditions</small></div></li>
            <li><span>06</span><div><strong>Simplify faults</strong><small>reduce delay and multiplicity</small></div></li>
            <li className={styles.finalShrink}><span>05</span><div><strong>Minimal counterexample</strong><small>same failure · 3 / 3</small></div></li>
          </ol>
        </div>
      </section>

      <section className={styles.routeCta}><div className="site-shell"><div><span>NEXT / REAL TRACE</span><h2>See the complete commit-then-close witness.</h2></div><Link className="button button-primary" href="/counterexamples/commit-then-close">Open the counterexample <span>↘</span></Link></div></section>
    </main>
  );
}
