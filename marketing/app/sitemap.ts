import type { MetadataRoute } from "next";

import { absoluteSiteUrl, primaryRoutes } from "@/lib/site";

export default function sitemap(): MetadataRoute.Sitemap {
  return primaryRoutes.map((route) => ({
    url: absoluteSiteUrl(route),
    changeFrequency: route === "/" ? "weekly" : "monthly",
    priority: route === "/" ? 1 : 0.8,
  }));
}
