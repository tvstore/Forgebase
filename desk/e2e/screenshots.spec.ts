// Screenshots of the desk for a pull request (UX.2). Skipped unless SCREENSHOTS=1, so CI is not
// affected. Run it against a server prepared by seed.sh (see CONTRIBUTING.md):
//
//   SCREENSHOTS=1 SCREENSHOT_DIR=/some/dir npx playwright test screenshots
//
// It adds a few sample documents (customers, and orders that are a draft, submitted and
// cancelled, so that every status badge shows), then takes the login page, home, list and form
// at 1280, 800 and 375 px (and the list at 320 px, a submitted form at 375 px), and asserts that
// no page scrolls sideways at those phone widths. The PNGs are not
// committed.

import { expect, test } from '@playwright/test';
import type { Page } from '@playwright/test';

const PASSWORD = process.env.E2E_PASSWORD ?? 'e2e-test-password';
const DIR = process.env.SCREENSHOT_DIR ?? 'test-results/screenshots';
const WIDTHS = [1280, 800, 375];

test.skip(process.env.SCREENSHOTS !== '1', 'set SCREENSHOTS=1 to take the screenshots');

async function noSideScroll(page: Page, what: string): Promise<void> {
  const w = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    client: document.documentElement.clientWidth,
  }));
  expect(w.scroll, `${what} scrolls sideways`).toBeLessThanOrEqual(w.client);
}

async function login(page: Page, username: string): Promise<void> {
  await page.goto('/desk/');
  await page.getByLabel('Username').fill(username);
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Log in' }).click();
  await expect(page.getByRole('heading', { name: 'Home', level: 1 })).toBeVisible();
}

/** Sample data through the API, as the logged-in manager (the desk's own session and CSRF token). */
async function seed(page: Page): Promise<{ draft: string; submitted: string; cancelled: string }> {
  return page.evaluate(async () => {
    const session = (await (await fetch('/api/v1/session', { headers: { 'X-Forgebase-CSRF': 'refresh' } })).json()) as {
      data: { csrf_token: string };
    };
    const send = async (method: string, path: string, body: unknown) => {
      const response = await fetch(`/api/v1${path}`, {
        method,
        headers: { 'Content-Type': 'application/json', 'X-Forgebase-CSRF': session.data.csrf_token },
        body: JSON.stringify(body),
      });
      const json = (await response.json().catch(() => ({}))) as { data?: { name: string; version: number } };
      return { ok: response.ok, doc: json.data };
    };
    const people = [
      ['grace@example.com', 'Grace Hopper', 'gold', 4200],
      ['alan@example.com', 'Alan Turing', 'silver', 1250],
      ['edsger@example.com', 'Edsger Dijkstra', 'standard', 300],
    ] as const;
    for (const [email, full_name, tier, loyalty_points] of people) {
      await send('POST', '/resource/customer', { email, full_name, tier, loyalty_points, active: true }); // a second run: already there
    }
    const order = async (customer: string, rows: [string, number, string][]) => {
      const created = await send('POST', '/resource/sales_order', {
        customer,
        order_date: '2026-10-29',
        items: rows.map(([description, qty, rate]) => ({ description, qty, rate })),
      });
      if (!created.doc) throw new Error('order not created');
      return created.doc;
    };
    const draft = await order('ada@example.com', [['Widget', 2, '9.50']]);
    const submitted = await order('grace@example.com', [
      ['Gadget', 10, '1250.00'],
      ['Cable', 4, '12.00'],
    ]);
    const done = await send('POST', `/resource/sales_order/${submitted.name}/submit`, { version: submitted.version });
    const toCancel = await order('alan@example.com', [['Sprocket', 100, '3.25']]);
    const s2 = await send('POST', `/resource/sales_order/${toCancel.name}/submit`, { version: toCancel.version });
    const c2 = await send('POST', `/resource/sales_order/${toCancel.name}/cancel`, { version: s2.doc?.version ?? 0 });
    if (!done.ok || !s2.ok || !c2.ok) throw new Error('could not submit or cancel the sample orders');
    return { draft: draft.name, submitted: submitted.name, cancelled: toCancel.name };
  });
}

test('screenshots of login, home, list and form', async ({ page }) => {
  // ---- login (logged out) -------------------------------------------------------------
  for (const width of WIDTHS) {
    await page.setViewportSize({ width, height: 800 });
    await page.goto('/desk/#/login');
    await expect(page.getByRole('button', { name: 'Log in' })).toBeVisible();
    await noSideScroll(page, `login at ${width}`);
    await page.screenshot({ path: `${DIR}/login-${width}.png` });
  }
  // A failed login shows the alert box.
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.getByLabel('Username').fill('e2e_manager');
  await page.getByLabel('Password').fill('wrong-password');
  await page.getByRole('button', { name: 'Log in' }).click();
  await expect(page.getByRole('alert')).toBeVisible();
  await page.screenshot({ path: `${DIR}/login-error-1280.png` });

  await login(page, 'e2e_manager');
  const docs = await seed(page);

  for (const width of WIDTHS) {
    await page.setViewportSize({ width, height: 800 });

    await page.goto('/desk/#/');
    await expect(page.getByRole('main').getByRole('link', { name: 'Sales Order', exact: true })).toBeVisible();
    await noSideScroll(page, `home at ${width}`);
    await page.screenshot({ path: `${DIR}/home-${width}.png` });

    await page.goto('/desk/#/list/sales_order');
    await expect(page.getByRole('row', { name: new RegExp(docs.submitted) })).toBeVisible();
    await expect(page.locator('[aria-busy="true"]')).toHaveCount(0);
    await noSideScroll(page, `list at ${width}`);
    await page.screenshot({ path: `${DIR}/list-${width}.png` });

    await page.goto(`/desk/#/form/sales_order/${encodeURIComponent(docs.draft)}`);
    await expect(page.locator('.status-badge')).toHaveText('Draft');
    await expect(page.getByLabel('Description, row 1')).toBeVisible();
    await noSideScroll(page, `form at ${width}`);
    await page.screenshot({ path: `${DIR}/form-${width}.png` });
  }

  // ---- more states at 1280 ------------------------------------------------------------
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/desk/#/list/customer');
  await expect(page.getByRole('row', { name: /Grace Hopper/ })).toBeVisible();
  await page.screenshot({ path: `${DIR}/list-customer-1280.png` });

  await page.goto('/desk/#/list/sales_order?q=zzz');
  await expect(page.getByRole('heading', { name: 'No documents match' })).toBeVisible();
  await page.screenshot({ path: `${DIR}/list-empty-1280.png` });

  await page.goto('/desk/#/list/sales_order');
  await page.getByRole('button', { name: 'Filters' }).click();
  await page.getByRole('button', { name: 'Add filter' }).click();
  await page.screenshot({ path: `${DIR}/list-filters-1280.png` });

  await page.goto(`/desk/#/form/sales_order/${encodeURIComponent(docs.submitted)}`);
  await expect(page.locator('.status-badge')).toHaveText('Submitted');
  await page.getByRole('button', { name: 'More actions' }).click();
  await page.screenshot({ path: `${DIR}/form-submitted-menu-1280.png` });
  await page.getByRole('button', { name: 'Cancel', exact: true }).click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await page.screenshot({ path: `${DIR}/form-dialog-1280.png` });
  await page.keyboard.press('Escape');

  await page.goto(`/desk/#/form/sales_order/${encodeURIComponent(docs.cancelled)}`);
  await expect(page.locator('.status-badge')).toHaveText('Cancelled');
  await page.screenshot({ path: `${DIR}/form-cancelled-1280.png` });

  await page.goto('/desk/#/new/sales_order');
  await expect(page.getByRole('heading', { name: 'New Sales Order' })).toBeVisible();
  await page.getByRole('button', { name: 'Save' }).click(); // empty: the required field is refused
  await expect(page.getByRole('alert')).toBeVisible();
  await page.screenshot({ path: `${DIR}/form-new-errors-1280.png` });

  await page.goto(`/desk/#/form/sales_order/${encodeURIComponent(docs.draft)}`);
  await page.getByLabel('Remarks').fill('Deliver on Friday.');
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.locator('.toast', { hasText: 'Saved' })).toBeVisible();
  await page.screenshot({ path: `${DIR}/form-saved-toast-1280.png` });

  // The smallest phone and a submitted form on a phone (the status badge sits under the title in the list).
  await page.setViewportSize({ width: 320, height: 800 });
  await page.goto('/desk/#/list/sales_order');
  await expect(page.getByRole('row', { name: new RegExp(docs.submitted) })).toBeVisible();
  await expect(page.locator('[aria-busy="true"]')).toHaveCount(0);
  await expect(page.locator('.toast')).toHaveCount(0, { timeout: 8000 }); // an earlier "Saved" is gone
  await noSideScroll(page, 'list at 320');
  await page.screenshot({ path: `${DIR}/list-320.png` });

  await page.setViewportSize({ width: 375, height: 800 });
  await page.goto(`/desk/#/form/sales_order/${encodeURIComponent(docs.submitted)}`);
  await expect(page.locator('.status-badge')).toHaveText('Submitted');
  await noSideScroll(page, 'submitted form at 375');
  await page.screenshot({ path: `${DIR}/form-submitted-375.png` });

  await page.getByRole('button', { name: 'Menu' }).click();
  await page.screenshot({ path: `${DIR}/form-menu-open-375.png` });
});
