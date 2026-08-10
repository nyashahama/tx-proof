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

test("counterexample instrument renders on its intended forensic ink surface", async ({ page }) => {
  await page.goto("/");

  const instrument = page.getByRole("region", {
    name: "Committed remotely. Unknown locally.",
  });
  await expect(instrument).toBeVisible();
  await expect(instrument).toHaveCSS("background-color", "rgb(16, 19, 16)");
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
