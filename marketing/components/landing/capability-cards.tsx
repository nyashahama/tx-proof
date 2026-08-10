import Link from "next/link";

import styles from "./capability-cards.module.css";

function ScheduleGraphic() {
  return (
    <div className={styles.scheduleGraphic} aria-hidden="true">
      <svg className={styles.orbitMap} viewBox="0 0 360 360">
        <defs>
          <linearGradient id="schedule-route" x1="40" y1="36" x2="320" y2="324">
            <stop stopColor="#625fff" />
            <stop offset="1" stopColor="#58c7ff" />
          </linearGradient>
          <radialGradient id="schedule-core">
            <stop stopColor="#7774ff" stopOpacity=".48" />
            <stop offset="1" stopColor="#625fff" stopOpacity="0" />
          </radialGradient>
        </defs>
        <circle className={styles.coreGlow} cx="180" cy="180" r="74" fill="url(#schedule-core)" />
        <circle className={styles.orbitRing} cx="180" cy="180" r="71" />
        <circle className={styles.orbitRing} cx="180" cy="180" r="124" />
        <path
          className={styles.routeLine}
          d="M79 105 C114 39 244 43 288 111 C332 179 287 302 184 307 C92 312 36 218 76 145 C107 89 187 85 230 128 C270 167 248 231 198 245"
          stroke="url(#schedule-route)"
        />
        <circle className={styles.routeSignal} cx="79" cy="105" r="5" />
      </svg>

      <div className={styles.intentCore}>
        <small>INTENT</small>
        <strong>op_41</strong>
        <span>one semantic operation</span>
      </div>

      <div className={`${styles.systemNode} ${styles.nodeCaller}`}>
        <span>01</span>
        <strong>CALLER</strong>
      </div>
      <div className={`${styles.systemNode} ${styles.nodeProvider}`}>
        <span>02</span>
        <strong>STRIPE</strong>
      </div>
      <div className={`${styles.systemNode} ${styles.nodeDatabase}`}>
        <span>03</span>
        <strong>POSTGRES</strong>
      </div>
      <div className={`${styles.systemNode} ${styles.nodeEffect}`}>
        <span>04</span>
        <strong>EFFECT</strong>
      </div>

      <div className={styles.graphicStatus}>
        <span>CAUSAL GRAPH</span>
        <strong>12 eligible actions</strong>
      </div>
    </div>
  );
}

function InvariantGraphic() {
  return (
    <div className={styles.invariantGraphic} aria-hidden="true">
      <div className={styles.queryChrome}>
        <span>SQL ORACLE / TERMINAL</span>
        <code>00:00.359</code>
      </div>
      <div className={styles.queryBody}>
        <div className={styles.queryText}>
          <span>SELECT</span> operation_id, count(*)
          <br />
          <span>FROM</span> fulfillments
          <br />
          <span>GROUP BY</span> operation_id
          <br />
          <span>HAVING</span> count(*) &gt; 1;
        </div>
        <div className={styles.scanLine} />
        <div className={styles.queryExpectation}>
          <span>EXPECTED</span>
          <strong>0 rows</strong>
          <i>property holds</i>
        </div>
        <div className={styles.witnessRow}>
          <div>
            <span>DIAGNOSTIC ROW / 01</span>
            <strong>op_41</strong>
          </div>
          <div>
            <span>FULFILLMENTS</span>
            <strong>2</strong>
          </div>
          <code>VIOLATION</code>
        </div>
      </div>
      <div className={styles.invariantFooter}>
        <span>FIVE APPROVED QUESTIONS</span>
        <span>YOUR DATABASE ANSWERS</span>
      </div>
    </div>
  );
}

function ShrinkGraphic() {
  return (
    <div className={styles.shrinkGraphic} aria-hidden="true">
      <div className={styles.shrinkSummary}>
        <div>
          <span>ORIGINAL</span>
          <strong>14</strong>
          <small>recorded actions</small>
        </div>
        <div className={styles.shrinkArrow}>
          <span />
          <i>VALIDITY-AWARE SHRINK</i>
          <span />
        </div>
        <div>
          <span>OWNED</span>
          <strong>05</strong>
          <small>decisive actions</small>
        </div>
      </div>

      <div className={styles.traceReduction}>
        <div className={styles.traceLabel}><span>FULL TRACE</span><code>14 / 14</code></div>
        <div className={styles.fullTrace}>
          {Array.from({ length: 14 }, (_, index) => (
            <span key={index} data-kept={[0, 3, 6, 10, 13].includes(index)} />
          ))}
        </div>
        <div className={styles.reductionLine}><span /></div>
        <div className={styles.traceLabel}><span>MINIMIZED</span><code>05 / 05</code></div>
        <div className={styles.minimalTrace}>
          {Array.from({ length: 5 }, (_, index) => (
            <span key={index}><i>0{index + 1}</i></span>
          ))}
        </div>
      </div>

      <div className={styles.identityProof}>
        <span className={styles.proofPulse} />
        <div><small>SAME FAILURE IDENTITY</small><strong>payment_fulfilled_at_most_once</strong></div>
        <code>3 / 3</code>
      </div>
    </div>
  );
}

const capabilities = [
  {
    index: "01",
    tag: "CAUSAL COMPILER",
    title: "Model the schedule",
    description:
      "Compile only state-valid provider outcomes, retries, deliveries, and one observable process kill—without inventing impossible histories.",
    href: "/method",
    linkLabel: "Explore the model",
    Graphic: ScheduleGraphic,
  },
  {
    index: "02",
    tag: "BUSINESS ORACLE",
    title: "Interrogate durable truth",
    description:
      "Ask five approved SQL invariants at causal checkpoints. Zero rows is a hold; a diagnostic row is the witness.",
    href: "/product",
    linkLabel: "Inspect the product",
    Graphic: InvariantGraphic,
  },
  {
    index: "03",
    tag: "REPLAY CONTRACT",
    title: "Own the regression",
    description:
      "Shrink the same failure across fresh baselines and export its replay, fingerprint, evidence, and CI result.",
    href: "/counterexamples/commit-then-close",
    linkLabel: "Open a regression",
    Graphic: ShrinkGraphic,
  },
];

export function CapabilityCards() {
  return (
    <section
      className={styles.section}
      aria-labelledby="capabilities-title"
    >
      <div className="site-shell">
        <header className={styles.header}>
          <p className="section-index">01 / FROM INTENT TO REGRESSION</p>
          <div>
            <h2 id="capabilities-title">How TxProof turns one intent into an owned regression</h2>
            <p>
              The product is one continuous chain: generate a valid history, ask the
              database what became true, then keep only the smallest failure that your
              team can replay.
            </p>
          </div>
        </header>

        <ul className={styles.grid} aria-label="TxProof capabilities">
          {capabilities.map(({ Graphic, ...capability }) => (
            <li key={capability.title}>
              <Link className={styles.cardLink} href={capability.href}>
                <article className={styles.card}>
                  <div className={styles.cardCopy}>
                    <div className={styles.cardMeta}>
                      <span>{capability.index}</span>
                      <code>{capability.tag}</code>
                    </div>
                    <h3>{capability.title}</h3>
                    <div className={styles.copyRule} />
                    <p>{capability.description}</p>
                    <span className={styles.cardAction}>
                      {capability.linkLabel}
                      <i aria-hidden="true">↗</i>
                    </span>
                  </div>
                  <Graphic />
                </article>
              </Link>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}
