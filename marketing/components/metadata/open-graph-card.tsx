import { ImageResponse } from "next/og";

import type { OpenGraphCard } from "@/content/open-graph";

export const openGraphSize = { width: 1200, height: 630 };
export const openGraphContentType = "image/png";

export function createOpenGraphCard(card: OpenGraphCard) {
  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          position: "relative",
          overflow: "hidden",
          background: "#08090a",
          color: "#f8f8f6",
          padding: "58px 64px",
          fontFamily: "Arial, sans-serif",
        }}
      >
        <div
          style={{
            position: "absolute",
            inset: 0,
            display: "flex",
            backgroundImage:
              "linear-gradient(rgba(255,255,255,0.045) 1px, transparent 1px), linear-gradient(90deg, rgba(255,255,255,0.045) 1px, transparent 1px)",
            backgroundSize: "64px 64px",
          }}
        />
        <div style={{ display: "flex", flexDirection: "column", width: 830 }}>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: 18,
              color: "#a9adb3",
              fontFamily: "monospace",
              fontSize: 17,
              letterSpacing: "0.14em",
            }}
          >
            <span>TXPROOF / {card.sequence}</span>
            <span style={{ width: 54, height: 1, display: "flex", background: "#484d54" }} />
            <span>{card.eyebrow}</span>
          </div>
          <div
            style={{
              display: "flex",
              marginTop: 66,
              maxWidth: 820,
              fontFamily: "Arial, sans-serif",
              fontSize: 72,
              fontWeight: 600,
              lineHeight: 0.95,
              letterSpacing: "-0.055em",
            }}
          >
            {card.title}
          </div>
          <div
            style={{
              display: "flex",
              marginTop: "auto",
              paddingTop: 34,
              borderTop: "1px solid rgba(255,255,255,0.18)",
              color: "#a9adb3",
              fontFamily: "monospace",
              fontSize: 18,
              letterSpacing: "0.035em",
            }}
          >
            {card.detail}
          </div>
        </div>
        <div
          style={{
            display: "flex",
            flexDirection: "column",
            justifyContent: "space-between",
            marginLeft: "auto",
            width: 230,
            paddingLeft: 40,
            borderLeft: "1px solid rgba(255,255,255,0.18)",
          }}
        >
          <span style={{ fontFamily: "monospace", fontSize: 14, color: "#858a91" }}>
            COUNTEREXAMPLE / RECEIPT
          </span>
          <span
            style={{
              display: "flex",
              color: "#f8f8f6",
              fontFamily: "Arial, sans-serif",
              fontSize: 132,
              fontWeight: 600,
              lineHeight: 0.8,
              letterSpacing: "-0.08em",
            }}
          >
            {card.sequence.padStart(2, "0")}
          </span>
          <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
            {[
              ["MODEL", "BOUND"],
              ["REPLAY", "3 / 3"],
              ["EXIT", "10"],
            ].map(([label, status], index) => (
              <div
                key={label}
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  paddingTop: 10,
                  borderTop: "1px solid rgba(255,255,255,0.16)",
                  color: index === 2 ? "#e05449" : "#a9adb3",
                  fontFamily: "monospace",
                  fontSize: 13,
                }}
              >
                <span>{label}</span><span>{status}</span>
              </div>
            ))}
          </div>
        </div>
      </div>
    ),
    openGraphSize,
  );
}
