import fs from "node:fs";
import path from "node:path";

import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { metadata as auditMetadata } from "@/app/audit/page";
import { metadata as counterexampleMetadata } from "@/app/counterexamples/commit-then-close/page";
import { metadata as methodMetadata } from "@/app/method/page";
import NotFound from "@/app/not-found";
import { metadata as productMetadata } from "@/app/product/page";
import { metadata as researchMetadata } from "@/app/research/page";
import robots from "@/app/robots";
import { metadata as safetyMetadata } from "@/app/safety/page";
import sitemap from "@/app/sitemap";
import {
  claimRecords,
  findExpiredClaims,
  findProhibitedPhrase,
  sourceRecords,
} from "@/content/claims";
import { openGraphCards } from "@/content/open-graph";
import { homeMetadata } from "@/lib/site";

const pages = [
  ["/", homeMetadata],
  ["/product", productMetadata],
  ["/method", methodMetadata],
  ["/safety", safetyMetadata],
  ["/research", researchMetadata],
  ["/counterexamples/commit-then-close", counterexampleMetadata],
  ["/audit", auditMetadata],
] as const;

function titleValue(metadata: (typeof pages)[number][1]) {
  return typeof metadata.title === "object" && metadata.title && "default" in metadata.title
    ? metadata.title.default
    : metadata.title;
}

describe("publishable site contract", () => {
  it("gives every primary route unique metadata and its own canonical path", () => {
    const titles = pages.map(([, metadata]) => titleValue(metadata));

    expect(new Set(titles).size).toBe(pages.length);
    for (const [path, metadata] of pages) {
      expect(metadata.description).toBeTruthy();
      expect(metadata.alternates?.canonical).toBe(path);
      expect(metadata.openGraph?.title).toBeTruthy();
      expect(metadata.openGraph?.description).toBeTruthy();
      expect(metadata.openGraph?.url).toBe(path);
    }
  });

  it("lists every primary route in the sitemap and exposes it through robots", () => {
    const sitemapPaths = sitemap().map((entry) => new URL(entry.url).pathname);

    expect(sitemapPaths).toEqual(pages.map(([path]) => path));
    expect(robots().sitemap).toBe("http://localhost:3000/sitemap.xml");
  });

  it("defines original, route-specific social artwork for every primary route", () => {
    const cards = Object.values(openGraphCards);

    expect(Object.keys(openGraphCards)).toEqual(pages.map(([path]) => path));
    expect(new Set(cards.map((card) => card.title)).size).toBe(cards.length);
    expect(new Set(cards.map((card) => card.sequence)).size).toBe(cards.length);
  });

  it("keeps asset and reference provenance reviewable", () => {
    const provenancePath = path.join(process.cwd(), "public", "provenance.json");
    const provenance = JSON.parse(fs.readFileSync(provenancePath, "utf8")) as {
      fonts: Array<{ family: string; source: string; license: string; licenseUrl: string }>;
      externalImageAssets: unknown[];
      referenceOnly: Array<{ name: string; adoption: string }>;
    };

    expect(provenance.fonts.map((font) => font.family)).toEqual([
      "Newsreader",
      "Manrope",
      "IBM Plex Mono",
    ]);
    expect(provenance.fonts.every((font) => font.source && font.license && font.licenseUrl)).toBe(true);
    expect(provenance.externalImageAssets).toEqual([]);
    expect(provenance.referenceOnly.find((reference) => reference.name === "Polar")?.adoption)
      .toMatch(/no copy/i);
  });

  it("gives an unknown route a deliberate recovery path", () => {
    render(NotFound());

    expect(screen.getByRole("heading", { level: 1, name: /This route is outside the model/i })).toBeVisible();
    expect(screen.getByRole("link", { name: /Return to the trace/i })).toHaveAttribute("href", "/");
  });

  it("keeps approved claims current, sourced, scoped, and free of prohibited language", () => {
    const approvedClaims = claimRecords.filter((claim) => claim.status === "approved");

    expect(findExpiredClaims(approvedClaims, new Date("2026-08-10T00:00:00Z"))).toEqual([]);
    expect(sourceRecords.length).toBeGreaterThan(0);

    for (const claim of approvedClaims) {
      expect(claim.sourceIds.length).toBeGreaterThan(0);
      expect(claim.scope).toBeTruthy();
      expect(findProhibitedPhrase(claim.copy)).toBeNull();
      for (const sourceId of claim.sourceIds) {
        expect(sourceRecords.some((source) => source.id === sourceId)).toBe(true);
      }
    }
  });

  it.each([
    "TxProof proves correctness",
    "Perfect deterministic replay for every run",
    "Exactly-once payments by construction",
  ])("detects prohibited release language: %s", (copy) => {
    expect(findProhibitedPhrase(copy)).not.toBeNull();
  });
});
