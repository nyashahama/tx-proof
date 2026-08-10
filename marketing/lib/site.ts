import type { Metadata } from "next";

export const primaryRoutes = [
  "/",
  "/product",
  "/method",
  "/safety",
  "/research",
  "/counterexamples/commit-then-close",
  "/audit",
] as const;

export type PrimaryRoute = (typeof primaryRoutes)[number];

export function getSiteUrl() {
  const configured = process.env.NEXT_PUBLIC_SITE_URL?.trim();
  const url = new URL(configured || "http://localhost:3000");

  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("NEXT_PUBLIC_SITE_URL must use http or https");
  }

  url.pathname = "/";
  url.search = "";
  url.hash = "";
  return url;
}

export function absoluteSiteUrl(path: string) {
  return new URL(path, getSiteUrl()).toString();
}

export function createPageMetadata(
  title: string,
  description: string,
  canonical: (typeof primaryRoutes)[number],
): Metadata {
  const socialTitle = `${title} — TxProof`;

  return {
    title,
    description,
    alternates: { canonical },
    openGraph: {
      title: socialTitle,
      description,
      type: "website",
      url: canonical,
    },
    twitter: {
      card: "summary_large_image",
      title: socialTitle,
      description,
    },
  };
}

export const homeMetadata: Metadata = {
  metadataBase: getSiteUrl(),
  title: {
    default: "TxProof — Counterexample search for money-moving backends",
    template: "%s — TxProof",
  },
  description:
    "Search Stripe, PostgreSQL, webhook, retry, and crash schedules for reproducible violations of your own money invariants.",
  alternates: { canonical: "/" },
  openGraph: {
    title: "TxProof — Find the schedule that makes your database lie about money",
    description:
      "Bounded counterexample search for Stripe and PostgreSQL backends. Local execution, minimized regressions, CI-native evidence.",
    type: "website",
    url: "/",
  },
  twitter: {
    card: "summary_large_image",
    title: "TxProof — Find the schedule that makes your database lie about money",
    description:
      "Bounded counterexample search for Stripe and PostgreSQL backends. Local execution, minimized regressions, CI-native evidence.",
  },
};
