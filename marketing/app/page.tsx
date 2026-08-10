import Link from "next/link";

import { TraceInstrument } from "@/components/hero/trace-instrument";

import styles from "./page.module.css";

const truths = [
  {
    index: "01",
    name: "Customer intent",
    state: "One checkout",
    detail: "The caller owns an operation ID, a retry decision, and the result it can actually observe.",
    mark: "op_41",
  },
  {
    index: "02",
    name: "Provider state",
    state: "Payment committed",
    detail: "Stripe can durably succeed even when the response never reaches your application.",
    mark: "pi_07",
  },
  {
    index: "03",
    name: "Application state",
    state: "Locally ambiguous",
    detail: "PostgreSQL contains the durable story your product, support team, and ledger will trust.",
    mark: "pay_203",
  },
  {
    index: "04",
    name: "Business effect",
    state: "Fulfilled twice",
    detail: "Entitlements, credits, shipments, refunds, and payouts are where contradiction becomes cost.",
    mark: "fx_2×",
  },
];

const faults = [
  ["Provider ambiguity", "commit_then_close", "orphan or duplicate payment"],
  ["Webhook delivery", "repeat · reorder · omit", "duplicate or regressed effect"],
  ["Caller retry", "same or changed key", "split operation identity"],
  ["Process lifecycle", "one observable SIGKILL", "commit without acknowledgement"],
  ["Reconciliation", "bounded horizon", "missing terminal convergence"],
];

export default function Home() {
  return (
    <main>
      <section className={styles.hero}>
        <div className={styles.heroGrid} aria-hidden="true" />
        <div className={`site-shell ${styles.heroInner}`}>
          <div className={styles.heroCopy}>
            <div className={styles.heroEyebrow}>
              <span>TXP / 00</span>
              <span>Counterexample search for money-moving backends</span>
            </div>
            <h1>
              Find the schedule that makes your database <em>lie about money.</em>
            </h1>
            <div className={styles.heroSupportRow}>
              <p className={styles.heroSupport}>
                TxProof searches valid Stripe, PostgreSQL, webhook, retry, and crash
                interleavings—then shrinks a real invariant failure into a regression
                your CI can own.
              </p>
              <div className={styles.heroActions}>
                <Link className="button button-primary" href="/counterexamples/commit-then-close">
                  View a failing trace
                  <span aria-hidden="true">↘</span>
                </Link>
                <Link className="button button-quiet" href="/audit">
                  Book a correctness audit
                  <span aria-hidden="true">↗</span>
                </Link>
              </div>
            </div>
            <p className={styles.qualifier}>
              <span aria-hidden="true">◎</span>
              Counterexample search, not proof.
            </p>
          </div>
          <div className={styles.instrumentWrap} id="failing-trace">
            <TraceInstrument />
          </div>
        </div>
      </section>

      <aside className={styles.evidenceRail} aria-label="Product evidence">
        <div className="site-shell">
          <div className={styles.evidenceItem}>
            <span>Execution</span>
            <strong>Local only</strong>
          </div>
          <Link className={`${styles.evidenceItem} ${styles.evidenceLink}`} href="/product">
            <span>Initial wedge</span>
            <strong>Stripe + PostgreSQL</strong>
          </Link>
          <div className={styles.evidenceItem}>
            <span>Business oracle</span>
            <strong>5 approved SQL invariants</strong>
          </div>
          <div className={styles.evidenceItem}>
            <span>Replay class</span>
            <strong>Fresh-baseline 3 / 3</strong>
          </div>
          <div className={styles.evidenceItem}>
            <span>Handoff</span>
            <strong>JSON · Markdown · JUnit</strong>
          </div>
        </div>
      </aside>

      <section className={`${styles.truthSection} section`} id="product">
        <div className="site-shell">
          <header className={styles.sectionHeader}>
            <p className="section-index">01 / THE CONTRADICTION</p>
            <div>
              <h2>Four systems can be individually right. Together, they can still be wrong.</h2>
              <p>
                A payment failure is rarely one broken component. It is a durable disagreement
                between facts committed by different owners at different times.
              </p>
            </div>
          </header>

          <div className={styles.truthMap}>
            <div className={styles.truthCore} aria-hidden="true">
              <span>semantic operation</span>
              <strong>op_41</strong>
              <small>one customer intent</small>
              <i />
            </div>
            <div className={styles.truthCards}>
              {truths.map((truth) => (
                <article className={styles.truthCard} key={truth.name}>
                  <div className={styles.truthCardTop}>
                    <span>{truth.index}</span>
                    <code>{truth.mark}</code>
                  </div>
                  <h3>{truth.name}</h3>
                  <strong>{truth.state}</strong>
                  <p>{truth.detail}</p>
                </article>
              ))}
            </div>
          </div>

          <div className={styles.contradictionNote}>
            <span className={styles.noteGlyph} aria-hidden="true">×</span>
            <p>
              <strong>The bug is the contradiction.</strong> The customer intended one purchase,
              Stripe committed once, the application lost certainty, and the downstream effect
              happened twice.
            </p>
            <Link href="/method">Study the failure model <span aria-hidden="true">↗</span></Link>
          </div>
        </div>
      </section>

      <section className={`${styles.methodSection} section`} id="method">
        <div className="site-shell">
          <header className={`${styles.sectionHeader} ${styles.sectionHeaderLight}`}>
            <p className="section-index section-index-light">02 / THE METHOD</p>
            <div>
              <h2>Declare the truth. Search the boundary. Keep the regression.</h2>
              <p>
                Not another suite of authored happy paths. TxProof compiles state-valid schedules,
                asks your disposable database a business question, and preserves the smallest
                counterexample that survives replay.
              </p>
            </div>
          </header>

          <div className={styles.methodRail}>
            <article>
              <div className={styles.methodNumber}>01</div>
              <div className={styles.methodGlyph} aria-hidden="true">
                <span>SELECT</span><span>0 rows</span>
              </div>
              <h3>Declare</h3>
              <p>Approve exactly five SQL invariants. Zero rows means the property holds; diagnostic rows are the witness.</p>
              <code>expect = &quot;zero_rows&quot;</code>
            </article>
            <article>
              <div className={styles.methodNumber}>02</div>
              <div className={`${styles.methodGlyph} ${styles.scheduleGlyph}`} aria-hidden="true">
                <span /><span /><span /><span /><span />
              </div>
              <h3>Search</h3>
              <p>Sample eligible provider outcomes, retries, deliveries, and one process kill from a seeded causal model.</p>
              <code>seed = 424242</code>
            </article>
            <article>
              <div className={styles.methodNumber}>03</div>
              <div className={`${styles.methodGlyph} ${styles.shrinkGlyph}`} aria-hidden="true">
                <span>14</span><i>→</i><span>5</span>
              </div>
              <h3>Shrink</h3>
              <p>Delete noise only when the same invariant fails at the same checkpoint on fresh baselines.</p>
              <code>identity preserved</code>
            </article>
          </div>

          <div className={styles.methodFooter}>
            <span>Compiled trace—not the seed—is replay authority.</span>
            <Link href="/method">Read the execution semantics <span aria-hidden="true">→</span></Link>
          </div>
        </div>
      </section>

      <section className={`${styles.faultSection} section`}>
        <div className="site-shell">
          <header className={styles.compactHeader}>
            <div>
              <p className="section-index">03 / CONTROLLED FAULT MODEL</p>
              <h2>Five boundaries. One causal schedule.</h2>
            </div>
            <p>
              TxProof coordinates the failures ordinary mocks isolate, while staying honest about
              the parts of your runtime it does not control.
            </p>
          </header>
          <div className={styles.faultTable} role="table" aria-label="Controlled fault model">
            <div className={styles.faultHeader} role="row">
              <span role="columnheader">Boundary</span>
              <span role="columnheader">Controlled variant</span>
              <span role="columnheader">Invariant at risk</span>
            </div>
            {faults.map(([boundary, variant, risk], index) => (
              <div className={styles.faultRow} role="row" key={boundary}>
                <span className={styles.faultIndex} aria-hidden="true">0{index + 1}</span>
                <strong role="cell">{boundary}</strong>
                <code role="cell">{variant}</code>
                <span role="cell">{risk}</span>
              </div>
            ))}
          </div>
        </div>
      </section>

      <section className={`${styles.artifactSection} section`}>
        <div className="site-shell">
          <header className={`${styles.sectionHeader} ${styles.sectionHeaderLight}`}>
            <p className="section-index section-index-light">04 / OWNED EVIDENCE</p>
            <div>
              <h2>One failure. Every artifact your team needs.</h2>
              <p>
                A bug report is not enough. The result is a compatibility-bound evidence package:
                original history, minimized trace, SQL witness, replay command, and a CI-native exit.
              </p>
            </div>
          </header>

          <div className={styles.artifactWorkbench}>
            <div className={styles.fileTree}>
              <div className={styles.panelLabel}><span>RUN ARTIFACT</span><code>run_7f31</code></div>
              <ul>
                <li><span>◫</span><strong>manifest.json</strong><small>compatibility fingerprint</small></li>
                <li><span>⌁</span><strong>trace.original.json</strong><small>14 actions · retained</small></li>
                <li className={styles.selectedFile}><span>⌁</span><strong>trace.minimized.json</strong><small>5 decisive actions</small></li>
                <li><span>≡</span><strong>summary.md</strong><small>redacted evidence</small></li>
                <li><span>✓</span><strong>junit.xml</strong><small>CI-compatible result</small></li>
                <li><span>#</span><strong>checksums.txt</strong><small>artifact integrity</small></li>
              </ul>
            </div>
            <div className={styles.artifactDetail}>
              <div className={styles.panelLabel}><span>MINIMIZED TRACE</span><code>BLAKE3 81e0…af4c</code></div>
              <div className={styles.codeLines} aria-label="Minimized trace excerpt">
                <div><span>01</span><code>client.post_checkout</code><small>operation op_41</small></div>
                <div><span>02</span><code>stripe.commit_then_close</code><small>pi_tiv_07</small></div>
                <div><span>03</span><code>webhook.deliver</code><small>evt_tiv_019</small></div>
                <div><span>04</span><code>compose.sigkill</code><small>after response</small></div>
                <div className={styles.failedLine}><span>05</span><code>webhook.retry</code><small>same event</small></div>
              </div>
              <div className={styles.replayCommand}>
                <span>$</span>
                <code>tiv replay .tiv/runs/run_7f31/trace.minimized.json</code>
                <span className={styles.commandTag} aria-hidden="true">REPLAY</span>
              </div>
            </div>
            <div className={styles.exitPanel}>
              <span className={styles.exitCode}>10</span>
              <div>
                <small>PROCESS EXIT</small>
                <strong>Reproducible invariant violation</strong>
              </div>
              <span className={styles.exitStatus}>CI / FAILED</span>
            </div>
          </div>
        </div>
      </section>

      <section className={`${styles.safetySection} section`} id="safety">
        <div className="site-shell">
          <header className={styles.sectionHeader}>
            <p className="section-index">05 / SAFETY BOUNDARY</p>
            <div>
              <h2>Destructive by design. Safe by refusal.</h2>
              <p>
                TxProof resets databases and kills processes. It proceeds only when every target is
                proven to be an isolated, marked, disposable local test environment.
              </p>
            </div>
          </header>

          <div className={styles.safetyGrid}>
            <div className={styles.identityCard}>
              <div className={styles.identityHeader}>
                <span>DATABASE IDENTITY / PREFLIGHT</span>
                <strong>mutation locked</strong>
              </div>
              <div className={styles.identityTarget}>
                <span className={styles.dbGlyph} aria-hidden="true">⌗</span>
                <div><small>TARGET</small><strong>tiv_case_checkout</strong></div>
                <code>127.0.0.1:5432</code>
              </div>
              <ul>
                <li><span>✓</span><div><strong>Loopback server</strong><small>Public database addresses rejected</small></div><code>PASS</code></li>
                <li><span>✓</span><div><strong>Disposable prefix</strong><small>Database begins with tiv_case_</small></div><code>PASS</code></li>
                <li><span>✓</span><div><strong>Verifier marker</strong><small>Marker UUID and database OID match</small></div><code>PASS</code></li>
                <li><span>✓</span><div><strong>Test provider key</strong><small>Live Stripe credentials rejected</small></div><code>PASS</code></li>
                <li><span>✓</span><div><strong>Size boundary</strong><small>84 MiB of 2 GiB maximum</small></div><code>PASS</code></li>
              </ul>
              <div className={styles.mutationState}><span>ALL 5 CHECKS HOLD</span><strong>Mutation enabled for exact identity</strong></div>
            </div>

            <div className={styles.safetyCopy}>
              <p className={styles.bigStatement}>
                If identity cannot be proven, the command exits <em>before mutation.</em>
              </p>
              <div className={styles.safetyRules}>
                <div><span>02</span><p><strong>Local artifacts.</strong> No source code or database dump leaves the customer environment by default.</p></div>
                <div><span>03</span><p><strong>Allowlist redaction.</strong> Keys, cookies, credentials, and raw environments never enter the artifact.</p></div>
                <div><span>04</span><p><strong>Bounded resources.</strong> Actions, connections, query time, run time, logs, and evidence size are capped.</p></div>
              </div>
              <Link className="text-link" href="/safety">Inspect the complete threat model <span aria-hidden="true">↗</span></Link>
            </div>
          </div>
        </div>
      </section>

      <section className={`${styles.boundarySection} section`}>
        <div className="site-shell">
          <div className={styles.boundaryLead}>
            <p className="section-index section-index-light">06 / THE HONEST BOUNDARY</p>
            <h2>Know exactly what this is—and what it isn’t.</h2>
            <p>
              Specificity is the trust signal. TxProof is a bounded counterexample search engine for
              one existing money-moving backend—not a claim to universal correctness.
            </p>
            <Link className="text-link text-link-light" href="/research">Review claims and sources <span aria-hidden="true">↗</span></Link>
          </div>
          <div className={styles.notList}>
            <div><span>NOT</span><strong>A Stripe clone</strong><small>Uses a narrow PaymentIntent fixture.</small></div>
            <div><span>NOT</span><strong>Production chaos</strong><small>Runs against disposable local state.</small></div>
            <div><span>NOT</span><strong>A formal proof system</strong><small>Search is bounded and model-dependent.</small></div>
            <div><span>NOT</span><strong>An observability platform</strong><small>Emits evidence for a regression.</small></div>
            <div><span>NOT</span><strong>A workflow runtime</strong><small>Attaches to the architecture you have.</small></div>
            <div className={styles.isItem}><span>IS</span><strong>An executable counterexample</strong><small>Your database. Your invariant. Your CI.</small></div>
          </div>
        </div>
      </section>

      <section className={styles.auditSection} id="audit">
        <div className="site-shell">
          <div className={styles.auditFrame}>
            <div className={styles.auditCopy}>
              <p className="section-index">07 / MONEY CORRECTNESS AUDIT</p>
              <h2>Bring one money flow. Leave with an owned regression.</h2>
              <p>
                A fixed-scope, local-only engagement for Stripe + PostgreSQL teams. Together we
                declare five invariants, run a bounded adversarial campaign, replay every credible
                finding, and hand the minimized regressions to your CI.
              </p>
              <div className={styles.auditActions}>
                <Link className="button button-primary" href="/audit">Qualify your repository <span aria-hidden="true">↗</span></Link>
                <span>5–10 business days · no production access</span>
              </div>
            </div>
            <div className={styles.auditReceipt}>
              <div className={styles.receiptTop}><span>SCOPE / 01 FLOW</span><code>LOCAL ONLY</code></div>
              <ol>
                <li><span>01</span><div><strong>Repository qualification</strong><small>Compose · PostgreSQL · PaymentIntent</small></div></li>
                <li><span>02</span><div><strong>Invariant workshop</strong><small>Five SQL properties · one horizon</small></div></li>
                <li><span>03</span><div><strong>Bounded campaign</strong><small>≤500 cases · ≤3 verified failures</small></div></li>
                <li><span>04</span><div><strong>Repair and handoff</strong><small>Replay · evidence · JUnit · CI</small></div></li>
              </ol>
              <div className={styles.receiptBottom}><span>DELIVERABLE</span><strong>Smallest reproducible counterexample</strong></div>
            </div>
          </div>
        </div>
      </section>
    </main>
  );
}
