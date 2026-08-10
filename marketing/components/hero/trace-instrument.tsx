"use client";

import { useEffect, useId, useState } from "react";

import {
  getScenario,
  minimizeScenario,
  scenarios,
  truthPlanes,
  type ScenarioId,
} from "@/lib/trace/scenarios";

import styles from "./trace-instrument.module.css";

const compactTraceQuery = "(max-width: 680px)";

function shouldCompactTrace() {
  return (
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia(compactTraceQuery).matches
  );
}

export function TraceInstrument() {
  const [activeId, setActiveId] = useState<ScenarioId>("lost-response");
  const [isMinimized, setIsMinimized] = useState(false);
  const titleId = useId();
  const activeScenario = getScenario(activeId);
  const minimized = minimizeScenario(activeScenario);

  useEffect(() => {
    if (typeof window.matchMedia !== "function") return;

    const viewport = window.matchMedia(compactTraceQuery);
    const syncDensity = () => setIsMinimized(viewport.matches);

    syncDensity();
    viewport.addEventListener("change", syncDensity);
    return () => viewport.removeEventListener("change", syncDensity);
  }, []);

  function selectScenario(id: ScenarioId) {
    setActiveId(id);
    setIsMinimized(shouldCompactTrace());
  }

  function moveTab(currentIndex: number, direction: -1 | 1) {
    const nextIndex =
      (currentIndex + direction + scenarios.length) % scenarios.length;
    const next = scenarios[nextIndex];
    selectScenario(next.id);
    document.getElementById(`scenario-${next.id}`)?.focus();
  }

  return (
    <section className={styles.instrument} aria-labelledby={titleId}>
      <div className={styles.instrumentBar}>
        <div className={styles.runIdentity}>
          <span className={styles.pulse} aria-hidden="true" />
          <span>ADVERSARIAL RUN</span>
          <span className={styles.muted}>seed 424242</span>
        </div>
        <div className={styles.runMeta} aria-label="Run metadata">
          <span>CASE 07/20</span>
          <span>LOCAL ONLY</span>
          <span>00:00.359</span>
        </div>
      </div>

      <div className={styles.scenarioRail}>
        <span className={styles.railLabel}>Inject one boundary</span>
        <div className={styles.tabs} role="tablist" aria-label="Failure scenario">
          {scenarios.map((scenario, index) => {
            const isActive = scenario.id === activeId;

            return (
              <button
                className={styles.tab}
                id={`scenario-${scenario.id}`}
                key={scenario.id}
                type="button"
                role="tab"
                aria-selected={isActive}
                aria-controls="trace-scenario-panel"
                tabIndex={isActive ? 0 : -1}
                onClick={() => selectScenario(scenario.id)}
                onKeyDown={(event) => {
                  if (event.key === "ArrowRight") {
                    event.preventDefault();
                    moveTab(index, 1);
                  }
                  if (event.key === "ArrowLeft") {
                    event.preventDefault();
                    moveTab(index, -1);
                  }
                  if (event.key === "Home") {
                    event.preventDefault();
                    selectScenario(scenarios[0].id);
                    document
                      .getElementById(`scenario-${scenarios[0].id}`)
                      ?.focus();
                  }
                  if (event.key === "End") {
                    event.preventDefault();
                    const last = scenarios.at(-1);
                    if (last) {
                      selectScenario(last.id);
                      document.getElementById(`scenario-${last.id}`)?.focus();
                    }
                  }
                }}
              >
                <span className={styles.tabIndex}>
                  0{index + 1}
                </span>
                {" "}
                {scenario.tabLabel}
              </button>
            );
          })}
        </div>
      </div>

      <div
        className={styles.scenarioPanel}
        id="trace-scenario-panel"
        role="tabpanel"
        aria-live="off"
      >
        <div className={styles.scenarioHeading}>
          <div>
            <p className={styles.resultLabel}>
              <span aria-hidden="true">×</span> INVARIANT VIOLATED
            </p>
            <h2 id={titleId}>{activeScenario.title}</h2>
            <p className={styles.scenarioSummary}>{activeScenario.summary}</p>
          </div>
          <dl className={styles.proofMeta}>
            <div>
              <dt>Reproduction</dt>
              <dd>{activeScenario.reproduction}</dd>
            </div>
            <div>
              <dt>Checkpoint</dt>
              <dd>{activeScenario.checkpoint}</dd>
            </div>
          </dl>
        </div>

        {isMinimized ? (
          <div className={styles.minimizedView}>
            <div className={styles.minimizedHeader}>
              <div>
                <span>VALIDITY-AWARE SHRINK</span>
                <strong>5 decisive actions</strong>
              </div>
              <span className={styles.shrinkRatio}>
                {activeScenario.events.length} → 5
              </span>
            </div>
            <ol aria-label="Minimized counterexample">
              {minimized.map((action) => (
                <li key={`${activeScenario.id}-${action.sequence}`}>
                  <span className={styles.actionNumber}>
                    {String(action.sequence).padStart(2, "0")}
                  </span>
                  <span>
                    <strong>{action.label}</strong>
                    <small>{action.detail}</small>
                  </span>
                </li>
              ))}
            </ol>
          </div>
        ) : (
          <div className={styles.traceView}>
            <div className={styles.timeAxis} aria-hidden="true">
              <span>REQUEST</span>
              <span>COMMIT</span>
              <span>DIVERGENCE</span>
              <span>WITNESS</span>
            </div>
            <div className={styles.truthGrid}>
              {truthPlanes.map((plane) => {
                const planeEvents = activeScenario.events.filter(
                  (event) => event.plane === plane.id,
                );

                return (
                  <div className={styles.truthRow} key={plane.id}>
                    <div className={styles.planeLabel}>
                      <span>{plane.shortLabel}</span>
                      <strong>{plane.label}</strong>
                    </div>
                    <ol aria-label={`${plane.label} events`}>
                      {planeEvents.map((event) => (
                        <li
                          className={styles.event}
                          data-tone={event.tone}
                          key={event.id}
                          style={{
                            "--event-position": `${
                              ((event.sequence - 1) /
                                Math.max(activeScenario.events.length - 1, 1)) *
                              82
                            }%`,
                          } as React.CSSProperties}
                        >
                          <span className={styles.eventDot} aria-hidden="true" />
                          <span className={styles.eventTime}>{event.time}</span>
                          <strong>{event.label}</strong>
                          <small>{event.detail}</small>
                        </li>
                      ))}
                    </ol>
                  </div>
                );
              })}
            </div>
            <div className={styles.eventCount}>
              {activeScenario.events.length} recorded events
            </div>
          </div>
        )}

        <div className={styles.witnessBar}>
          <div className={styles.witnessIdentity}>
            <span>FAILED INVARIANT</span>
            <strong>{activeScenario.invariant}</strong>
          </div>
          <div className={styles.witnessText}>
            <span>SQL WITNESS</span>
            <strong>{activeScenario.witness}</strong>
          </div>
          <button
            className={styles.minimizeButton}
            type="button"
            onClick={() => setIsMinimized((current) => !current)}
          >
            <span aria-hidden="true">{isMinimized ? "↗" : "↘"}</span>
            {isMinimized ? "Show full trace" : "Minimize trace"}
          </button>
        </div>
      </div>

      <p className={styles.srOnly} role="status" aria-live="polite">
        {activeScenario.tabLabel} scenario selected. Invariant {activeScenario.invariant}{" "}
        violated.
      </p>
    </section>
  );
}
