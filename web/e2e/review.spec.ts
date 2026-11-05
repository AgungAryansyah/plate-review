import { expect, test } from "@playwright/test";

test("sign in, open the review workspace, and view keyboard shortcuts", async ({ page }) => {
  const account = { id: "user-1", username: "reviewer", role: "reviewer" };
  await page.route("**/api/auth/me", (route) => route.fulfill({ status: 401, json: { error: "Sign in to continue" } }));
  await page.route("**/api/auth/login", (route) => route.fulfill({ status: 200, json: { user: account } }));
  await page.route("**/api/videos**", (route) => route.fulfill({ status: 200, json: [] }));
  await page.route("**/api/exports", (route) => route.fulfill({ status: 200, json: [] }));

  await page.goto("/");
  await page.getByLabel("Username").fill("reviewer");
  await page.getByLabel("Password").fill("correct horse battery");
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("heading", { name: "Recordings" })).toBeVisible();
  await page.keyboard.press("?");
  await expect(page.getByRole("dialog", { name: "Review without reaching for the mouse." })).toBeVisible();
});
