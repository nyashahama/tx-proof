import Link from "next/link";

import { TraceInstrument } from "@/components/hero/trace-instrument";
import { createPageMetadata } from "@/lib/site";

import styles from "../../technical.module.css";

export const metadata = createPageMetadata(
  "Commit-then-close counterexample",
  "A complete modeled TxProof failure: original schedule, invariant witness, replay, shrink history, and repaired result.",
  "/counterexamples/commit-then-close",
);

const invariantSql = [
  "select event_id, count(*) as effects",
  "from fulfilments",
  "where event_id = 'evt_tiv_019'",
  "group by event_id",
  "having count(*) > 1;",
].join("\n");

export default function CounterexamplePage() {
  return (
    <main className={styles.page}>
      <section className={`${styles.hero} ${styles.counterHero}`}><div className="site-shell"><p className={styles.kicker}>COUNTEREXAMPLE / CE-001</p><div className={styles.heroGrid}>
        <div className={styles.heroCopy}><h1>One customer intent. Two fulfilments.</h1><p>Stripe commits. The response closes. The success webhook is handled. The API dies before acknowledgement. The same event, <strong>evt_tiv_019</strong>, is retried and creates a second durable effect.</p><div className={styles.heroLinks}><Link className="button button-primary" href="#trace">Inspect the trace <span>↘</span></Link></div></div>
        <div className={styles.caseStamp}><span>FAILURE IDENTITY</span><strong>webhook-effect-<br/>at-most-once</strong><dl><div><dt>checkpoint</dt><dd>after_webhook_retry</dd></div><div><dt>reproduction</dt><dd>3 / 3</dd></div><div><dt>exit</dt><dd>10</dd></div></dl></div>
      </div></div></section>

      <section className={styles.traceSection} id="trace"><div className="site-shell"><TraceInstrument /></div></section>

      <section className="section"><div className="site-shell">
        <header className={styles.sectionHeading}><span>01 / PRECONDITION + ORACLE</span><h2>The database owns the business verdict.</h2><p>The provider fixture supplies immutable external evidence. One read-only repeatable-read PostgreSQL snapshot decides whether the invariant returns a witness.</p></header>
        <div className={styles.oracleSplit}>
          <div><span>INVARIANT SQL</span><pre><code>{invariantSql}</code></pre></div>
          <div><span>DIAGNOSTIC ROW</span><table><thead><tr><th>event_id</th><th>effects</th></tr></thead><tbody><tr><td>evt_tiv_019</td><td>2</td></tr></tbody></table><p>Zero rows means hold. This one row is the actionable witness.</p></div>
        </div>
      </div></section>

      <section className={`${styles.darkSection} section`}><div className="site-shell">
        <header className={`${styles.sectionHeading} ${styles.headingDark}`}><span>02 / SHRINK HISTORY</span><h2>Fourteen actions enter. Five causal actions remain.</h2><p>Amount, currency, object identity, invariant, and checkpoint never change.</p></header>
        <div className={styles.shrinkHistory}>
          <div><span>candidate 00</span><strong>14</strong><small>original retained</small></div><i>→</i>
          <div><span>candidate 07</span><strong>09</strong><small>delete idle waits</small></div><i>→</i>
          <div><span>candidate 13</span><strong>06</strong><small>remove caller retry</small></div><i>×</i>
          <div><span>candidate 14</span><strong>05</strong><small>identity preserved</small></div>
        </div>
        <p className={styles.boundaryCallout}>Rejected candidate 13 stopped reproducing the same witness. The shrinker restored the required action and kept searching.</p>
      </div></section>

      <section className="section"><div className="site-shell">
        <header className={styles.sectionHeading}><span>03 / REPAIRED REPLAY</span><h2>The same trace becomes a normal regression.</h2><p>The application inserts event identity before applying the effect and treats duplicate delivery as an acknowledged no-op.</p></header>
        <div className={styles.repairCompare}>
          <div className={styles.beforeRepair}><span>BEFORE</span><strong>event → effect → record</strong><small>crash window permits replayed effect</small><code>EXIT 10</code></div>
          <div className={styles.afterRepair}><span>AFTER</span><strong>claim event → effect → acknowledge</strong><small>same event ID cannot own two effects</small><code>EXIT 0</code></div>
        </div>
      </div></section>

      <section className={styles.routeCta}><div className="site-shell"><div><span>NEXT / YOUR FLOW</span><h2>Search one repository for the failure its current tests miss.</h2></div><Link className="button button-primary" href="/audit">Qualify a repository <span>↗</span></Link></div></section>
    </main>
  );
}
