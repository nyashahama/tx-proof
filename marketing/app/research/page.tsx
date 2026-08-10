import Link from "next/link";

import { claimRecords, sourceRecords } from "@/content/claims";
import { createPageMetadata } from "@/lib/site";

import styles from "../technical.module.css";

export const metadata = createPageMetadata(
  "Research and claims",
  "The provider evidence, method lineage, competitive boundary, and prohibited claims behind TxProof.",
  "/research",
);

const sources = sourceRecords.filter((source) => source.public);
const boundedVerdict = claimRecords.find((claim) => claim.id === "CL-014");

export default function ResearchPage() {
  return (
    <main className={styles.page}>
      <section className={`${styles.hero} ${styles.heroDark}`}>
        <div className="site-shell">
          <p className={styles.kicker}>SOURCE LEDGER / CLAIM CONTROL</p>
          <div className={styles.heroGrid}>
            <div className={styles.heroCopy}>
              <h1>Trust the method because every claim has a boundary.</h1>
              <p>TxProof separates documented provider behavior, prior art, competitive hypotheses, and prohibited language. Every factual statement carries a source, scope, date, and status.</p>
              <div className={styles.heroLinks}><Link className="button button-primary" href="#ledger">Open the source ledger <span>↘</span></Link></div>
            </div>
            <div className={styles.claimCard}>
              <div className={styles.cardBar}><span>CLAIM / CL-014</span><code>APPROVED</code></div>
              <blockquote>“{boundedVerdict?.copy}”</blockquote>
              <dl><div><dt>Scope</dt><dd>one configured campaign</dd></div><div><dt>Excludes</dt><dd>proof of correctness</dd></div><div><dt>Reviewed</dt><dd>2026-08-10</dd></div></dl>
            </div>
          </div>
        </div>
      </section>

      <section className="section" id="ledger"><div className="site-shell">
        <header className={styles.sectionHeading}><span>01 / SOURCE LEDGER</span><h2>Research cut-off: 10 August 2026.</h2><p>Claims that may drift are dated and reviewed again before publication. Source IDs keep the marketing surface tied to the engineering blueprint.</p></header>
        <div className={styles.sourceLedger}>
          <div className={styles.ledgerHeader}><span>ID</span><span>Authority</span><span>Evidence</span><span>Status</span></div>
          {sources.map((source) => (
            <article key={source.id}>
              <code>{source.id}</code>
              <a
                aria-label={`${source.authority} ${source.topic}`}
                href={source.url}
                rel="noreferrer"
              >
                <strong>{source.authority}</strong>
                <small>{source.topic}</small>
              </a>
              <p>{source.evidence}</p>
              <span>{source.status.replace("-", " ").toUpperCase()}</span>
            </article>
          ))}
        </div>
        <p className={styles.researchNote}>Primary links were rechecked on 10 August 2026. The fuller URL register and evidence excerpts remain in the product blueprint; no unsourced competitor assertion ships.</p>
      </div></section>

      <section className={`${styles.darkSection} section`}><div className="site-shell">
        <header className={`${styles.sectionHeading} ${styles.headingDark}`}><span>02 / CLAIM CLASSES</span><h2>Documented fact is not the same thing as benchmark evidence.</h2><p>The interface makes uncertainty visible instead of laundering it through confident copy.</p></header>
        <div className={styles.claimClasses}>
          <article><span className={styles.statusPrimary}>DOCUMENTED</span><h3>Provider behavior</h3><p>Directly supported by current primary documentation and stated within its exact scope.</p></article>
          <article><span className={styles.statusEvidence}>MEASURED</span><h3>Benchmark result</h3><p>Produced by a dated, reproducible head-to-head protocol with environment and limitations.</p></article>
          <article><span className={styles.statusHypothesis}>HYPOTHESIS</span><h3>Competitive gap</h3><p>Public material does not show the capability; hands-on validation is still required.</p></article>
          <article><span className={styles.statusProhibited}>PROHIBITED</span><h3>Universal guarantee</h3><p>“Proves correctness,” “exactly-once payments,” and “perfect deterministic replay” never ship.</p></article>
        </div>
      </div></section>

      <section className="section"><div className="site-shell">
        <header className={styles.sectionHeading}><span>03 / HEAD-TO-HEAD RULE</span><h2>Win a buyer-valued dimension—or stop.</h2><p>The next investment is a benchmark against the same deliberately buggy and real repositories, not a broad product build.</p></header>
        <div className={styles.benchmarkGrid}>
          {[["Setup", "minutes to a valid baseline"], ["Findings", "non-trivial bugs discovered"], ["Noise", "false positives and oracle errors"], ["Replay", "fresh-baseline stability"], ["Minimality", "smallest valid trace"], ["Ownership", "result retained in CI"]].map(([metric, detail], index) => <div key={metric}><span>0{index + 1}</span><strong>{metric}</strong><small>{detail}</small></div>)}
        </div>
      </div></section>

      <section className={styles.routeCta}><div className="site-shell"><div><span>NEXT / INSPECTABLE EVIDENCE</span><h2>Read the failure from precondition to repaired replay.</h2></div><Link className="button button-primary" href="/counterexamples/commit-then-close">Open the counterexample <span>↘</span></Link></div></section>
    </main>
  );
}
