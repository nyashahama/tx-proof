import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

test("homepage explains TxProof and exposes a working counterexample", async ({ page }) => {
  await page.goto("/");

  await expect(page).toHaveTitle(/TxProof/);
  await expect(
    page.getByRole("heading", {
      level: 1,
      name: "Find the schedule that makes your database lie about money.",
    }),
  ).toBeVisible();
  await expect(page.getByText("Counterexample search, not proof.")).toBeVisible();

  await page.getByRole("tab", { name: "02 Duplicate webhook" }).click();
  await expect(
    page.getByRole("heading", { level: 2, name: "One event. Two durable effects." }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Minimize trace" }).click();
  await expect(page.getByRole("list", { name: "Minimized counterexample" }).getByRole("listitem"))
    .toHaveCount(5);
});

test("homepage renders with the Polar-aligned dark presentation contract", async ({ page }) => {
  await page.goto("/");

  await expect(page.locator("body")).toHaveCSS("background-color", "rgb(9, 9, 9)");

  const heroHeading = page.getByRole("heading", {
    level: 1,
    name: "Find the schedule that makes your database lie about money.",
  });
  await expect(heroHeading).toHaveCSS("text-align", "center");

  const instrument = page.getByRole("region", {
    name: "Committed remotely. Unknown locally.",
  });
  await expect(instrument).toBeVisible();
  await expect(instrument).toHaveCSS("background-color", "rgb(17, 17, 19)");

  const primaryAction = page.getByRole("link", { name: "View a failing trace" });
  await expect(primaryAction).toHaveCSS("background-color", "rgb(245, 246, 250)");
  await expect(primaryAction).toHaveCSS("color", "rgb(9, 9, 9)");
  await expect(primaryAction).toHaveCSS("border-radius", "999px");
});

test("homepage leads from three visual capabilities into live product evidence", async ({ page }) => {
  await page.goto("/");

  const capabilities = page.getByRole("region", {
    name: "How TxProof turns one intent into an owned regression",
  });
  await expect(capabilities.getByRole("listitem")).toHaveCount(3);

  const capabilityBox = await capabilities.boundingBox();
  const traceBox = await page
    .getByRole("region", { name: "Committed remotely. Unknown locally." })
    .boundingBox();

  expect(capabilityBox).not.toBeNull();
  expect(traceBox).not.toBeNull();
  expect(capabilityBox!.y).toBeLessThan(traceBox!.y);
});

test("operating principle progressively resolves and remains readable", async ({ page }) => {
  await page.goto("/");

  const principle = page.getByTestId("operating-principle");
  await principle.scrollIntoViewIfNeeded();
  await page.waitForFunction(() =>
    [...document.querySelectorAll('[data-testid="operating-principle"] [data-vision-word]')]
      .some((word) => word.getAttribute("data-active") === "true"),
  );

  await expect(
    principle.getByRole("heading", {
      level: 2,
      name: "Your money flow stays local. The failure becomes evidence your team can keep.",
    }),
  ).toBeVisible();
});

test("homepage has no serious automated accessibility violations", async ({ page }) => {
  await page.goto("/");

  const results = await new AxeBuilder({ page }).analyze();
  const serious = results.violations.filter((violation) =>
    violation.impact === "serious" || violation.impact === "critical",
  );

  expect(serious).toEqual([]);
});

test("homepage reflows without horizontal overflow at 390px", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");

  const widths = await page.evaluate(() => ({
    viewport: document.documentElement.clientWidth,
    content: document.documentElement.scrollWidth,
  }));

  expect(widths.content).toBeLessThanOrEqual(widths.viewport);
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
  await expect(page.getByRole("tab", { name: "01 Lost response" })).toBeVisible();
});

test("mobile opens the trace as a concise five-action counterexample", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");

  const instrument = page.getByRole("region", {
    name: "Committed remotely. Unknown locally.",
  });
  await expect(
    instrument.getByRole("list", { name: "Minimized counterexample" }).getByRole("listitem"),
  ).toHaveCount(5);

  await instrument.getByRole("button", { name: "Show full trace" }).click();
  await expect(instrument.getByText("9 recorded events")).toBeVisible();
  await expect(instrument.getByRole("button", { name: "Minimize trace" })).toBeVisible();
});

test("counterexample stays understandable and operable with reduced motion", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("/");

  await page.getByRole("tab", { name: "04 Process crash" }).click();
  await expect(
    page.getByRole("heading", { level: 2, name: "The row commits. The acknowledgement does not." }),
  ).toBeVisible();

  await page.getByRole("button", { name: "Minimize trace" }).click();
  await expect(page.getByRole("list", { name: "Minimized counterexample" }).getByRole("listitem"))
    .toHaveCount(5);

  const principleWords = page.getByTestId("operating-principle").locator("[data-vision-word]");
  await expect(principleWords).toHaveCount(13);
  await expect(
    page
      .getByTestId("operating-principle")
      .locator('[data-vision-word][data-active="true"]'),
  ).toHaveCount(13);
});
