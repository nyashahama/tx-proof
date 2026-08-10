import type { Metadata } from "next";
import Link from "next/link";

import styles from "./technical.module.css";

export const metadata: Metadata = {
  title: "Route not found",
  description: "The requested TxProof route is outside the published product model.",
  robots: { index: false, follow: false },
};

export default function NotFound() {
  return (
    <main className={styles.page}>
      <section className={`${styles.hero} ${styles.heroDark}`}>
        <div className="site-shell">
          <p className={styles.kicker}>404 / UNCOMPILED ROUTE</p>
          <div className={styles.heroGrid}>
            <div className={styles.heroCopy}>
              <h1>This route is outside the model.</h1>
              <p>
                The requested path does not belong to the published TxProof surface. Return to the
                canonical trace or inspect the method behind it.
              </p>
              <div className={styles.heroLinks}>
                <Link className="button button-primary" href="/">Return to the trace <span>↘</span></Link>
                <Link className="text-link text-link-light" href="/method">Read the method <span>↗</span></Link>
              </div>
            </div>
            <div className={styles.claimCard}>
              <div className={styles.cardBar}><span>ROUTE / UNKNOWN</span><code>EXIT 404</code></div>
              <blockquote>“No state transition exists for this path.”</blockquote>
              <dl>
                <div><dt>Observed</dt><dd>unregistered route</dd></div>
                <div><dt>Mutation</dt><dd>none</dd></div>
                <div><dt>Recovery</dt><dd>safe navigation</dd></div>
              </dl>
            </div>
          </div>
        </div>
      </section>
    </main>
  );
}
