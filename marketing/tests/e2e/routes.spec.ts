import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

const routes = [
  ["/", "Find the schedule that makes your database lie about money."],
  ["/product", "Control the boundary. Preserve the evidence."],
  ["/method", "Search the schedules your happy path never chooses."],
  ["/safety", "Nothing moves until the target proves it is disposable."],
  ["/research", "Trust the method because every claim has a boundary."],
  ["/counterexamples/commit-then-close", "One customer intent. Two fulfilments."],
  ["/audit", "A correctness audit for one money flow."],
] as const;

for (const [path, heading] of routes) {
  test(`${path} keeps the Polar-aligned dark canvas`, async ({ page }) => {
    await page.goto(path);

    const canvas = path === "/"
      ? page.locator("body")
      : page.locator("#main-content > *").first();

    await expect(canvas).toHaveCSS(
      "background-color",
      "rgb(9, 9, 9)",
    );
    await expect(page.getByRole("heading", { level: 1, name: heading })).toHaveCSS(
      "color",
      "rgb(245, 246, 250)",
    );
  });

  test(`${path} has its route contract and no serious accessibility finding`, async ({ page }) => {
    await page.goto(path);

    await expect(page.getByRole("heading", { level: 1, name: heading })).toBeVisible();
    await expect(page).toHaveTitle(/TxProof/);

    const results = await new AxeBuilder({ page }).analyze();
    const serious = results.violations.filter((violation) =>
      violation.impact === "serious" || violation.impact === "critical",
    );

    expect(serious).toEqual([]);
  });

  test(`${path} reflows at 390px`, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(path);

    const widths = await page.evaluate(() => ({
      viewport: document.documentElement.clientWidth,
      content: document.documentElement.scrollWidth,
    }));

    expect(widths.content).toBeLessThanOrEqual(widths.viewport);
    await expect(page.getByRole("heading", { level: 1, name: heading })).toBeVisible();
  });
}

test("every primary route publishes canonical and route-specific social metadata", async ({ page }) => {
  const socialImages = new Set<string>();

  for (const [path] of routes) {
    await page.goto(path);

    const canonical = await page.locator('link[rel="canonical"]').getAttribute("href");
    const socialImage = await page.locator('meta[property="og:image"]').getAttribute("content");

    expect(canonical).toBeTruthy();
    expect(new URL(canonical!).pathname).toBe(path);
    expect(socialImage).toBeTruthy();
    socialImages.add(new URL(socialImage!).pathname);

    const imageUrl = new URL(socialImage!);
    const response = await page.request.get(`${imageUrl.pathname}${imageUrl.search}`);
    expect(response.ok()).toBe(true);
    expect(response.headers()["content-type"]).toContain("image/png");
  }

  expect(socialImages.size).toBe(routes.length);
});

test("crawler endpoints expose the primary route set", async ({ request }) => {
  const sitemapResponse = await request.get("/sitemap.xml");
  const sitemap = await sitemapResponse.text();
  const robotsResponse = await request.get("/robots.txt");
  const robots = await robotsResponse.text();

  expect(sitemapResponse.ok()).toBe(true);
  expect(robotsResponse.ok()).toBe(true);
  for (const [path] of routes) {
    expect(sitemap).toContain(path === "/" ? "http://localhost:3000/</loc>" : `${path}</loc>`);
  }
  expect(robots).toContain("Allow: /");
  expect(robots).toContain("Sitemap: http://localhost:3000/sitemap.xml");
});

test("an unknown route returns a deliberate and accessible 404", async ({ page }) => {
  const response = await page.goto("/schedules/outside-the-model");

  expect(response?.status()).toBe(404);
  await expect(page.getByRole("heading", { level: 1, name: "This route is outside the model." }))
    .toBeVisible();
  await expect(page.getByRole("link", { name: /Return to the trace/i })).toHaveAttribute("href", "/");

  const results = await new AxeBuilder({ page }).analyze();
  const serious = results.violations.filter((violation) =>
    violation.impact === "serious" || violation.impact === "critical",
  );
  expect(serious).toEqual([]);
});
