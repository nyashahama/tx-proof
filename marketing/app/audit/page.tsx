import Link from "next/link";

import { createPageMetadata } from "@/lib/site";

import styles from "../technical.module.css";

export const metadata = createPageMetadata(
  "Money Correctness Audit",
  "A fixed-scope, local-only verification audit for one Stripe and PostgreSQL money flow.",
  "/audit",
);

const timeline = [
  ["01", "Qualify", "One repository, one PaymentIntent flow, a disposable Compose environment, and a named engineering owner."],
  ["02", "Declare", "Five approved invariants, a quiescence predicate, and one explicit reconciliation horizon."],
  ["03", "Search", "A bounded adversarial campaign with provider, webhook, retry, crash, and database checkpoints."],
  ["04", "Reproduce", "Every credible finding classified on three fresh baselines; inconclusive results remain inconclusive."],
  ["05", "Repair", "Customer engineer fixes the application while the minimized trace remains the acceptance test."],
  ["06", "Handoff", "Threat note, invariant pack, original and minimized traces, evidence, replay, JUnit, and CI configuration."],
];

export default function AuditPage() {
  return (
    <main className={styles.page}>
      <section className={`${styles.hero} ${styles.auditHero}`}><div className="site-shell"><p className={styles.kicker}>FIXED-SCOPE / LOCAL-ONLY</p><div className={styles.heroGrid}>
        <div className={styles.heroCopy}><h1>A correctness audit for one money flow.</h1><p>Pair with TxProof inside your environment. Declare <strong>Five approved invariants</strong>, run a bounded campaign, inspect every reproducible counterexample, and leave the minimized failures in CI.</p><div className={styles.heroLinks}><Link className="button button-primary" href="#qualification">Check repository fit <span>↘</span></Link><Link className="button button-quiet" href="/safety">Review the safety boundary <span>↗</span></Link></div></div>
        <div className={styles.scopeReceipt}><div className={styles.cardBar}><span>ENGAGEMENT / 01</span><code>5–10 DAYS</code></div><dl><div><dt>Repository</dt><dd>one</dd></div><div><dt>Money flow</dt><dd>one</dd></div><div><dt>Invariants</dt><dd>five</dd></div><div><dt>Campaign</dt><dd>≤500 cases</dd></div><div><dt>Findings</dt><dd>≤3 verified</dd></div><div><dt>Production access</dt><dd>none</dd></div></dl><p>Promise: rigorous search and evidence—not a guaranteed defect and never proof.</p></div>
      </div></div></section>

      <section className="section"><div className="site-shell">
        <header className={styles.sectionHeading}><span>01 / ENGAGEMENT FLOW</span><h2>From repository fit to owned regression.</h2><p>Each stage has an explicit decision, evidence boundary, and customer owner.</p></header>
        <ol className={styles.auditTimeline}>{timeline.map(([index, title, description]) => <li key={title}><span>{index}</span><div><h3>{title}</h3><p>{description}</p></div></li>)}</ol>
      </div></section>

      <section className={`${styles.darkSection} section`} id="qualification"><div className="site-shell">
        <header className={`${styles.sectionHeading} ${styles.headingDark}`}><span>02 / QUALIFICATION</span><h2>The audit begins only when the seams are real.</h2><p>No production credentials, no copied customer data, and no vague “test our payments” scope.</p></header>
        <div className={styles.qualificationGrid}>
          <article><span>01</span><h3>Can the backend run through Docker Compose?</h3><p>Services need deterministic health and an isolated project identity.</p></article>
          <article><span>02</span><h3>Can the Stripe SDK use a test base URL?</h3><p>This seam is required to model committed effect with a lost response safely.</p></article>
          <article><span>03</span><h3>Is PostgreSQL disposable and marked?</h3><p>The exact case database must be safe to drop and recreate repeatedly.</p></article>
          <article><span>04</span><h3>Can the team approve five SQL invariants?</h3><p>A payments owner must agree what zero rows means and what evidence is useful.</p></article>
          <article><span>05</span><h3>Is one checkout request expressible?</h3><p>One HTTP operation template captures the semantic IDs the schedule needs.</p></article>
          <article><span>06</span><h3>Is quiescence observable?</h3><p>Workers need a repository-owned predicate for no queued or running work.</p></article>
        </div>
      </div></section>

      <section className="section"><div className="site-shell">
        <header className={styles.sectionHeading}><span>03 / DELIVERABLE BUNDLE</span><h2>Evidence for leadership. Regression assets for engineering.</h2><p>The engagement finishes when results are classified, limitations are explicit, and reproducible failures can run without the audit team.</p></header>
        <div className={styles.deliverableGrid}>
          <div><span>01</span><strong>Threat + scope note</strong><small>model, budget, exclusions, residual risk</small></div>
          <div><span>02</span><strong>Invariant pack</strong><small>five reviewed SQL files and evidence columns</small></div>
          <div><span>03</span><strong>Counterexamples</strong><small>original + minimized traces with replay identity</small></div>
          <div><span>04</span><strong>Evidence report</strong><small>redacted provider, local, and compatibility facts</small></div>
          <div><span>05</span><strong>Repair workshop</strong><small>customer-owned fix against the smallest trace</small></div>
          <div><span>06</span><strong>CI handoff</strong><small>JUnit, exit codes, replay, and scheduled search</small></div>
        </div>
        <div className={styles.intakeNote}><span>INTAKE STATUS</span><div><strong>The public contact channel is intentionally not fabricated.</strong><p>Connect the confirmed founder-led audit address before publication. Until then, this build documents qualification and scope without presenting a dead form.</p></div></div>
      </div></section>

      <section className={styles.routeCta}><div className="site-shell"><div><span>BEFORE CONTACT</span><h2>See the exact kind of regression the audit returns.</h2></div><Link className="button button-primary" href="/counterexamples/commit-then-close">View the canonical result <span>↘</span></Link></div></section>
    </main>
  );
}
