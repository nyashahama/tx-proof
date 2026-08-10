import Link from "next/link";

import styles from "./shell.module.css";

const links = [
  ["Product", "/product"],
  ["Method", "/method"],
  ["Safety", "/safety"],
  ["Research", "/research"],
] as const;

function BrandMark() {
  return (
    <span className={styles.brandMark} aria-hidden="true">
      <i /><i /><i /><i />
    </span>
  );
}

export function SiteHeader() {
  return (
    <header className={styles.header}>
      <div className={`site-shell ${styles.headerInner}`}>
        <Link className={styles.brand} href="/" aria-label="TxProof home">
          <BrandMark />
          <strong>TxProof</strong>
          <span>transactional invariant verifier</span>
        </Link>
        <nav className={styles.desktopNav} aria-label="Primary navigation">
          {links.map(([label, href]) => <Link href={href} key={href}>{label}</Link>)}
          <Link className={styles.traceLink} href="/counterexamples/commit-then-close">Counterexample <span>↘</span></Link>
        </nav>
        <Link className={styles.headerCta} href="/audit">Book an audit <span aria-hidden="true">↗</span></Link>
        <details className={styles.mobileNav}>
          <summary aria-label="Open navigation"><span /><span /></summary>
          <nav aria-label="Mobile navigation">
            {links.map(([label, href]) => <Link href={href} key={href}>{label}</Link>)}
            <Link href="/counterexamples/commit-then-close">Counterexample</Link>
            <Link href="/audit">Book an audit</Link>
          </nav>
        </details>
      </div>
    </header>
  );
}
