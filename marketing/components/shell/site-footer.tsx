import Link from "next/link";

import styles from "./shell.module.css";

export function SiteFooter() {
  return (
    <footer className={styles.footer}>
      <div className="site-shell">
        <div className={styles.footerTop}>
          <div className={styles.footerBrand}>
            <span className={styles.footerLogo}><span aria-hidden="true">TXP</span></span>
            <p>Counterexample search for money-moving backends.</p>
          </div>
          <div className={styles.footerLinks}>
            <div><strong>Product</strong><Link href="/product">Product</Link><Link href="/method">Method</Link><Link href="/safety">Safety</Link></div>
            <div><strong>Evidence</strong><Link href="/research">Research</Link><Link href="/counterexamples/commit-then-close">Counterexample</Link><Link href="/audit">Audit</Link></div>
          </div>
        </div>
        <div className={styles.footerBottom}>
          <span>© 2026 TxProof. Working product identity.</span>
          <span>Research cut-off: 10 August 2026</span>
          <strong>Passing means no violation found under this model and budget—not correctness.</strong>
        </div>
      </div>
    </footer>
  );
}
