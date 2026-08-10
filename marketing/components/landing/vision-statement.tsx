"use client";

import { useEffect, useRef } from "react";

import styles from "./vision-statement.module.css";

const statement =
  "Your money flow stays local. The failure becomes evidence your team can keep.";
const words = statement.split(" ");

export function VisionStatement() {
  const rootRef = useRef<HTMLElement>(null);

  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;

    const wordNodes = Array.from(
      root.querySelectorAll<HTMLElement>("[data-vision-word]"),
    );
    const reducedMotion = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    let animationFrame: number | null = null;

    const markProgress = (progress: number) => {
      const bounded = Math.min(1, Math.max(0, progress));
      const activeCount = Math.ceil(bounded * wordNodes.length);

      root.style.setProperty("--vision-progress", String(bounded));
      wordNodes.forEach((word, index) => {
        word.dataset.active = String(index < activeCount);
      });
    };

    const update = () => {
      if (reducedMotion?.matches) {
        markProgress(1);
        return;
      }

      const rect = root.getBoundingClientRect();
      const viewportHeight = window.innerHeight;
      const start = viewportHeight * 0.72;
      const travel = Math.max(rect.height - viewportHeight * 0.78, 1);
      markProgress((start - rect.top) / travel);
    };

    const scheduleUpdate = () => {
      if (animationFrame !== null) return;
      animationFrame = window.requestAnimationFrame(() => {
        animationFrame = null;
        update();
      });
    };

    update();
    window.addEventListener("scroll", scheduleUpdate, { passive: true });
    window.addEventListener("resize", scheduleUpdate);
    reducedMotion?.addEventListener?.("change", scheduleUpdate);

    return () => {
      window.removeEventListener("scroll", scheduleUpdate);
      window.removeEventListener("resize", scheduleUpdate);
      reducedMotion?.removeEventListener?.("change", scheduleUpdate);
      if (animationFrame !== null) window.cancelAnimationFrame(animationFrame);
    };
  }, []);

  return (
    <section
      className={styles.section}
      data-testid="operating-principle"
      aria-labelledby="operating-principle-title"
      ref={rootRef}
    >
      <div className={styles.stickyFrame}>
        <div className={`site-shell ${styles.layout}`}>
          <div className={styles.progressColumn} aria-hidden="true">
            <span>LOCAL</span>
            <div className={styles.progressTrack}><i /></div>
            <span>OWNED</span>
          </div>
          <div className={styles.statementColumn}>
            <p className="section-index">07 / OPERATING PRINCIPLE</p>
            <h2 id="operating-principle-title">
              <span className={styles.srOnly}>{statement}</span>
              <span className={styles.visibleWords} aria-hidden="true">
                {words.map((word, index) => (
                  <span
                    data-vision-word
                    data-active="true"
                    key={`${word}-${index}`}
                  >
                    {word}{" "}
                  </span>
                ))}
              </span>
            </h2>
            <div className={styles.promiseGrid}>
              <div><span>01</span><strong>No production access</strong><small>Disposable local state only</small></div>
              <div><span>02</span><strong>No source upload</strong><small>Artifacts stay in your environment</small></div>
              <div><span>03</span><strong>No vague finding</strong><small>Replay, witness, fingerprint, CI exit</small></div>
            </div>
          </div>
        </div>
      </div>
    </section>
  );
}
