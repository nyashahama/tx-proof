import Link from "next/link";

import { createPageMetadata } from "@/lib/site";

import styles from "../technical.module.css";

export const metadata = createPageMetadata(
  "Safety boundary",
  "The exact local-only identity, credential, mutation, redaction, and resource controls required by TxProof.",
  "/safety",
);

const gates = [
  ["01", "Database name", "tiv_case_* prefix", "PASS"],
  ["02", "Server address", "loopback only", "PASS"],
  ["03", "Identity binding", "OID + owner + marker UUID", "PASS"],
  ["04", "Compose project", "exact isolated project", "PASS"],
  ["05", "Provider credentials", "test key only", "PASS"],
  ["06", "Data and size", "synthetic · ≤2 GiB", "PASS"],
];

export default function SafetyPage() {
  return (
    <main className={styles.page}>
      <section className={styles.hero}>
        <div className="site-shell"><p className={styles.kicker}>NON-NEGOTIABLE SAFETY BOUNDARY</p><div className={styles.heroGrid}>
          <div className={styles.heroCopy}><h1>Nothing moves until the target proves it is disposable.</h1><p>TxProof is destructive inside one exact local test identity. A live key, public host, wrong marker, identity drift, or oversized database terminates the command before any mutation.</p><div className={styles.heroLinks}><Link className="button button-primary" href="#preflight">Inspect preflight <span>↘</span></Link></div></div>
          <div className={styles.lockCard}><div className={styles.lockIcon} aria-hidden="true"><span>×</span></div><small>DEFAULT STATE</small><strong>Mutation locked</strong><p>six independent proofs required</p><code>exit 2 / safety preflight</code></div>
        </div></div>
      </section>

      <section className="section" id="preflight"><div className="site-shell">
        <header className={styles.sectionHeading}><span>01 / EXACT IDENTITY</span><h2>Exit before mutation unless all six proofs hold.</h2><p>The reset acknowledgement binds to the same database facts checked immediately before every destructive candidate.</p></header>
        <div className={styles.preflightCard}>
          <div className={styles.preflightTarget}><div><small>RESOLVED TARGET</small><strong>tiv_case_checkout</strong></div><code>server 4df2…19a · oid 16391</code></div>
          <div className={styles.gateList}>{gates.map(([index, name, value, status]) => <div key={name}><span>{index}</span><strong>{name}</strong><code>{value}</code><em>{status}</em></div>)}</div>
          <div className={styles.safeResult}><span>✓ ALL PROOFS HOLD</span><strong>Mutation enabled for this identity only</strong></div>
        </div>
      </div></section>

      <section className={`${styles.darkSection} section`}><div className="site-shell">
        <header className={`${styles.sectionHeading} ${styles.headingDark}`}><span>02 / REFUSAL MATRIX</span><h2>Unsafe inputs fail closed. No override by convenience.</h2><p>A safety error is not an inconclusive product result; the campaign never started.</p></header>
        <div className={styles.refusalGrid}>
          <article><span>LIVE KEY</span><h3>Provider credential rejected</h3><p>Test-mode provider identity must be positively established.</p><code>before fixture start</code></article>
          <article><span>PUBLIC IP</span><h3>Database host rejected</h3><p>Managed, shared, and non-loopback targets are outside the v0 boundary.</p><code>before DB connection</code></article>
          <article><span>MARKER DRIFT</span><h3>Database identity rejected</h3><p>Name alone is insufficient; marker UUID, OID, owner, server, and project must match.</p><code>before reset</code></article>
          <article><span>SECRET OUTPUT</span><h3>Artifact field rejected</h3><p>Persisted evidence is allowlisted, redacted, capped, and written local mode 0600.</p><code>before artifact write</code></article>
        </div>
      </div></section>

      <section className="section"><div className="site-shell">
        <header className={styles.sectionHeading}><span>03 / LOCAL DATA BOUNDARY</span><h2>Your repository and database stay where they are.</h2><p>Early audits pair inside the customer environment. Source code, database dumps, raw environments, and production data are not collected.</p></header>
        <div className={styles.dataBoundary}>
          <div className={styles.localCircle}><span>LOCAL HOST</span><strong>source<br/>database<br/>artifacts</strong><small>mode 0600</small></div>
          <div className={styles.blockedArrow}><span>×</span><strong>NO DEFAULT EXPORT</strong></div>
          <div className={styles.externalCircle}><span>OUTSIDE</span><strong>production<br/>PAN data<br/>live Stripe</strong><small>prohibited</small></div>
        </div>
      </div></section>

      <section className={styles.routeCta}><div className="site-shell"><div><span>NEXT / ENGAGEMENT</span><h2>Review the same boundary before an audit begins.</h2></div><Link className="button button-primary" href="/audit">Read the audit scope <span>↗</span></Link></div></section>
    </main>
  );
}
