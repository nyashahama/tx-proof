import Link from "next/link";

import { createPageMetadata } from "@/lib/site";

import styles from "../technical.module.css";

export const metadata = createPageMetadata(
  "Product contract",
  "The controlled system, invariant oracle, replay model, and artifact contract behind TxProof.",
  "/product",
);

const invariants = [
  ["01", "provider-object-unique", "One semantic operation cannot split into multiple provider or local payment objects."],
  ["02", "webhook-effect-at-most-once", "One provider event cannot create the same business effect more than once."],
  ["03", "amount-currency-consistent", "Provider, payment, order, and effect agree in integer minor units and currency."],
  ["04", "terminal-state-monotonic", "A terminal success cannot regress when an older event arrives later."],
  ["05", "ledger-balanced / entitlement-safe", "Value balances—or entitlement is granted once, and only after success."],
];

export default function ProductPage() {
  return (
    <main className={styles.page}>
      <section className={styles.hero}>
        <div className="site-shell">
          <p className={styles.kicker}>PRODUCT CONTRACT / V0</p>
          <div className={styles.heroGrid}>
            <div className={styles.heroCopy}>
              <h1>Control the boundary. Preserve the evidence.</h1>
              <p>
                TxProof attaches to one disposable Stripe + PostgreSQL checkout flow. It controls
                the external schedule, evaluates your database, and returns an executable
                counterexample—not a dashboard and never a proof claim.
              </p>
              <div className={styles.heroLinks}>
                <Link className="button button-primary" href="/counterexamples/commit-then-close">Open the canonical counterexample <span>↘</span></Link>
                <Link className="button button-quiet" href="/method">Read the method <span>↗</span></Link>
              </div>
            </div>
            <div className={styles.commandCard}>
              <div className={styles.cardBar}><span>TIV / RUN</span><code>seed 424242</code></div>
              <div className={styles.commandPrompt}><span>$</span><code>tiv run --seed 424242 --cases 20 --ci</code></div>
              <dl>
                <div><dt>Invariant</dt><dd>webhook-effect-at-most-once</dd></div>
                <div><dt>Witness</dt><dd>evt_tiv_019 → 2 fulfilments</dd></div>
                <div><dt>Replay</dt><dd>3 / 3 fresh baselines</dd></div>
                <div><dt>Minimal</dt><dd>5 decisive actions</dd></div>
              </dl>
              <div className={styles.commandResult}><span>EXIT 10</span><strong>Reproducible violation</strong></div>
            </div>
          </div>
        </div>
      </section>

      <section className="section">
        <div className="site-shell">
          <header className={styles.sectionHeading}>
            <span>01 / CONTROLLED SYSTEM</span>
            <h2>One operation. Three controlled surfaces. Your application remains real.</h2>
            <p>The harness coordinates a narrow provider fixture, your Compose application, and an exact disposable database identity.</p>
          </header>
          <div className={styles.systemMap}>
            <div className={styles.systemNode}><span>CONTROL PLANE</span><strong>tiv CLI</strong><small>schedule · replay · shrink</small></div>
            <i aria-hidden="true">→</i>
            <div className={`${styles.systemNode} ${styles.nodeAccent}`}><span>YOUR CODE</span><strong>Compose backend</strong><small>real handlers · workers · schema</small></div>
            <i aria-hidden="true">↔</i>
            <div className={styles.systemStack}>
              <div className={styles.systemNode}><span>PROVIDER FIXTURE</span><strong>PaymentIntent</strong><small>stateful · idempotent · signed events</small></div>
              <div className={styles.systemNode}><span>BUSINESS ORACLE</span><strong>PostgreSQL</strong><small>repeatable-read · read-only</small></div>
            </div>
          </div>
          <div className={styles.factStrip}>
            <div><span>Runtime</span><strong>Docker Compose</strong></div>
            <div><span>Database</span><strong>PostgreSQL ≤ 2 GiB</strong></div>
            <div><span>Provider surface</span><strong>PaymentIntent v1</strong></div>
            <div><span>Process fault</span><strong>One observable SIGKILL</strong></div>
          </div>
        </div>
      </section>

      <section className={`${styles.darkSection} section`}>
        <div className="site-shell">
          <header className={`${styles.sectionHeading} ${styles.headingDark}`}>
            <span>02 / BUSINESS ORACLE</span>
            <h2>Exactly five invariants. Approved by the people who own the money flow.</h2>
            <p>Each trusted SQL file returns zero rows when its property holds and diagnostic rows when it fails.</p>
          </header>
          <div className={styles.invariantList}>
            {invariants.map(([index, id, description]) => (
              <article key={id}><span>{index}</span><code>{id}</code><p>{description}</p><strong>ZERO ROWS</strong></article>
            ))}
          </div>
          <p className={styles.boundaryCallout}>A final pass/fail decision is deterministic. An LLM may suggest a mapping during setup; it never judges money correctness.</p>
        </div>
      </section>

      <section className="section">
        <div className="site-shell">
          <header className={styles.sectionHeading}>
            <span>03 / ARTIFACT CONTRACT</span>
            <h2>A result is useful only when it survives handoff.</h2>
            <p>The manifest binds a failure to the exact tool, repository, configuration, database identity, and evidence hashes that produced it.</p>
          </header>
          <div className={styles.artifactGrid}>
            <div className={styles.fingerprintCard}>
              <div className={styles.cardBar}><span>COMPATIBILITY FINGERPRINT</span><code>v1</code></div>
              <ul>
                <li><span>repository</span><code>commit + dirty-tree hash</code></li>
                <li><span>environment</span><code>images + OS + architecture</code></li>
                <li><span>oracle</span><code>5 invariant hashes</code></li>
                <li><span>database</span><code>server + OID + marker UUID</code></li>
                <li><span>trace</span><code>compiled BLAKE3 hash</code></li>
              </ul>
              <div className={styles.hashLine}>81e0e4b75b9b…af4c</div>
            </div>
            <div className={styles.artifactCopy}>
              <h3>Original forever. Minimal when proven.</h3>
              <p>The original schedule is immutable. Shrinking writes a second trace only when the same invariant fails at the same checkpoint in at least two of three fresh-baseline attempts.</p>
              <ul>
                <li>Redacted provider and local evidence</li>
                <li>Markdown and JSON summaries</li>
                <li>JUnit result and distinct exit code</li>
                <li>Checksums and replay command</li>
              </ul>
            </div>
          </div>
        </div>
      </section>

      <section className={styles.routeCta}>
        <div className="site-shell">
          <div><span>NEXT / QUALIFICATION</span><h2>Does your repository expose the seams TxProof needs?</h2></div>
          <Link className="button button-primary" href="/audit">Qualify a repository <span>↗</span></Link>
        </div>
      </section>
    </main>
  );
}
