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
          background: "#111210",
          color: "#f2efe6",
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
              "linear-gradient(rgba(242,239,230,0.055) 1px, transparent 1px), linear-gradient(90deg, rgba(242,239,230,0.055) 1px, transparent 1px)",
            backgroundSize: "64px 64px",
          }}
        />
        <div style={{ display: "flex", flexDirection: "column", width: 830 }}>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: 18,
              color: "#ff5a36",
              fontFamily: "monospace",
              fontSize: 17,
              letterSpacing: "0.14em",
            }}
          >
            <span>TXPROOF / {card.sequence}</span>
            <span style={{ width: 54, height: 1, display: "flex", background: "#ff5a36" }} />
            <span>{card.eyebrow}</span>
          </div>
          <div
            style={{
              display: "flex",
              marginTop: 66,
              maxWidth: 820,
              fontFamily: "Georgia, serif",
              fontSize: 70,
              lineHeight: 0.96,
              letterSpacing: "-0.045em",
            }}
          >
            {card.title}
          </div>
          <div
            style={{
              display: "flex",
              marginTop: "auto",
              paddingTop: 34,
              borderTop: "1px solid rgba(242,239,230,0.28)",
              color: "#aeb0a8",
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
            width: 212,
            paddingLeft: 40,
            borderLeft: "1px solid rgba(242,239,230,0.28)",
          }}
        >
          <span style={{ fontFamily: "monospace", fontSize: 14, color: "#8f9289" }}>
            TRACE / MINIMAL
          </span>
          <span
            style={{
              display: "flex",
              color: "#ff5a36",
              fontFamily: "Georgia, serif",
              fontSize: 144,
              lineHeight: 0.8,
              letterSpacing: "-0.08em",
            }}
          >
            {card.sequence}
          </span>
          <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
            {["COMPILE", "REPLAY 3/3", "SHRINK 05"].map((label, index) => (
              <div
                key={label}
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  paddingTop: 10,
                  borderTop: "1px solid rgba(242,239,230,0.22)",
                  color: index === 2 ? "#ff5a36" : "#aeb0a8",
                  fontFamily: "monospace",
                  fontSize: 13,
                }}
              >
                <span>{label}</span><span>OK</span>
              </div>
            ))}
          </div>
        </div>
      </div>
    ),
    openGraphSize,
  );
}
