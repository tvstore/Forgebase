// The smoke test of the desk (M4.2): login -> list -> create -> edit -> submit on the example
// `sales_order` DocType (submittable, naming series, a child table, a Link to `customer`), then a
// user without the submit right. It runs against a real server prepared by seed.sh and checks the
// M4 exit criteria that need a browser: no secret in browser storage, and an action the user has
// no right for is hidden (the API refusal itself is tested in Rust: forgebase-http/tests/resources.rs
// and crates/forgebase-db/tests/permissions.rs; one refused request is repeated here).

import { expect, test } from '@playwright/test';
import type { Page } from '@playwright/test';

const PASSWORD = process.env.E2E_PASSWORD ?? 'e2e-test-password';
const CUSTOMER = 'ada@example.com';

async function login(page: Page, username: string): Promise<void> {
  await page.goto('/desk/');
  await page.getByLabel('Username').fill(username);
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Log in' }).click();
  await expect(page.getByRole('heading', { name: 'Document types' })).toBeVisible();
}

async function logout(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'Log out' }).click();
  await expect(page.getByRole('button', { name: 'Log in' })).toBeVisible();
}

/** The only secret the desk may leave in the browser is the HttpOnly cookie, which scripts cannot see. */
async function expectNoSecretsInStorage(page: Page): Promise<void> {
  const stored = await page.evaluate(() => ({
    local: window.localStorage.length,
    session: window.sessionStorage.length,
    cookie: document.cookie,
  }));
  expect(stored).toEqual({ local: 0, session: 0, cookie: '' });
  const cookies = await page.context().cookies();
  const session = cookies.find((c) => c.name === 'fb_session');
  expect(session?.httpOnly).toBe(true);
}

/** From the sales order list: opens a new sales order and fills it with one row; leaves it unsaved. */
async function fillNewOrder(page: Page): Promise<void> {
  await page.getByRole('link', { name: 'New' }).click();
  await expect(page.getByRole('heading', { name: 'New Sales Order' })).toBeFocused();

  // The Link field suggests the names of the target DocType as you type.
  await page.getByLabel('customer').fill('ada');
  await expect(page.locator(`datalist option[value="${CUSTOMER}"]`)).toHaveCount(1);
  await page.getByLabel('customer').fill(CUSTOMER);
  await page.getByLabel('order_date').fill('2026-10-29');

  await page.getByRole('button', { name: 'Add row' }).click();
  await expect(page.getByLabel('description, row 1')).toBeFocused();
  await page.getByLabel('description, row 1').fill('Widget');
  await page.getByLabel('qty, row 1').fill('2');
  await page.getByLabel('rate, row 1').fill('9.50');
}

test('login, list, create, edit and submit a sales order; a clerk cannot submit', async ({ page }) => {
  // ---- login and list -------------------------------------------------------------------
  await login(page, 'e2e_manager');
  await expectNoSecretsInStorage(page);
  await page.getByRole('link', { name: 'Sales Order' }).click();
  await expect(page.getByRole('heading', { name: 'Sales Order', level: 1 })).toBeVisible();
  await expect(page.getByRole('status')).toHaveText(''); // the list has loaded (it may hold orders of an earlier run)

  // ---- create ---------------------------------------------------------------------------
  await fillNewOrder(page);

  // A row with a non-integer quantity is refused on its cell, and nothing is created.
  await page.getByLabel('qty, row 1').fill('1.5');
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByRole('alert')).toContainText('not saved');
  await expect(page.getByLabel('qty, row 1')).toBeFocused();
  await expect(page.getByLabel('qty, row 1')).toHaveAttribute('aria-invalid', 'true');
  await expect(page.getByText('Enter a whole number.')).toBeVisible();
  await page.getByLabel('qty, row 1').fill('2');

  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page).toHaveURL(/#\/form\/sales_order\/SO-\d{4}-\d{5}$/);
  const name = decodeURIComponent(page.url().split('/').pop() ?? '');
  await expect(page.getByRole('heading', { name, level: 1 })).toBeFocused();
  await expect(page.locator('.doc-status')).toHaveText('Status: Draft');
  await expect(page.getByLabel('customer')).toHaveValue(CUSTOMER);
  await expect(page.getByLabel('rate, row 1')).toHaveValue('9.50');
  await expect(page.getByRole('button', { name: 'Submit' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Delete' })).toBeVisible();

  // ---- edit: only the changed field is sent ---------------------------------------------
  await page.getByLabel('remarks').fill('first remark');
  const patch = page.waitForRequest((r) => r.method() === 'PATCH');
  await page.getByRole('button', { name: 'Save' }).click();
  expect(JSON.parse((await patch).postData() ?? '{}')).toEqual({ version: 1, data: { remarks: 'first remark' } });
  await expect(page.getByRole('status')).toHaveText('Saved.');
  await expect(page.locator('dd', { hasText: /^2$/ })).toBeVisible();

  // Unsaved changes are not lost by navigating away: the page asks first.
  await page.getByLabel('remarks').fill('unsaved remark');
  await page.getByRole('link', { name: 'Home' }).click();
  await expect(page.getByRole('group', { name: 'You have unsaved changes.' })).toBeVisible();
  await page.getByRole('button', { name: 'Stay' }).click();
  await expect(page).toHaveURL(/#\/form\/sales_order\//);
  await expect(page.getByLabel('remarks')).toHaveValue('unsaved remark');
  await page.getByLabel('remarks').fill('first remark');

  // ---- submit ---------------------------------------------------------------------------
  await page.getByRole('button', { name: 'Submit' }).click();
  await expect(page.locator('.doc-status')).toHaveText('Status: Submitted');
  await expect(page.getByRole('button', { name: 'Submit' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Delete' })).toHaveCount(0); // a submitted document cannot be deleted
  // A submitted order edits only its allow_on_submit field; its table is read-only.
  await expect(page.getByLabel('remarks')).toBeEditable();
  await expect(page.getByLabel('customer')).not.toBeEditable();
  await expect(page.getByRole('button', { name: 'Add row' })).toHaveCount(0);
  await expectNoSecretsInStorage(page);

  // The order is in the list.
  await page.getByRole('link', { name: 'Back to Sales Order list' }).click();
  await expect(page.getByRole('link', { name })).toBeVisible();
  await logout(page);

  // ---- a user without the submit right --------------------------------------------------
  await login(page, 'e2e_clerk');
  await page.getByRole('link', { name: 'Sales Order' }).click();
  await page.getByRole('link', { name }).click();
  await expect(page.locator('.doc-status')).toHaveText('Status: Submitted');
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0); // no cancel right
  await expect(page.getByRole('button', { name: 'Amend' })).toHaveCount(0);

  await page.getByRole('link', { name: 'Back to Sales Order list' }).click();
  await fillNewOrder(page);
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page).toHaveURL(/#\/form\/sales_order\/SO-\d{4}-\d{5}$/);
  const draft = decodeURIComponent(page.url().split('/').pop() ?? '');
  await expect(page.locator('.doc-status')).toHaveText('Status: Draft');
  await expect(page.getByRole('button', { name: 'Save' })).toBeVisible(); // the clerk can work on the draft
  await expect(page.getByRole('button', { name: 'Submit' })).toHaveCount(0); // but not submit it
  await expect(page.getByRole('button', { name: 'Delete' })).toHaveCount(0);

  // The API refuses what the desk hides (the Rust tests cover every right; this is one request).
  const status = await page.evaluate(async (documentName) => {
    const session = (await (await fetch('/api/v1/session', { headers: { 'X-Forgebase-CSRF': 'refresh' } })).json()) as {
      data: { csrf_token: string };
    };
    const response = await fetch(`/api/v1/resource/sales_order/${encodeURIComponent(documentName)}/submit`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-Forgebase-CSRF': session.data.csrf_token },
      body: JSON.stringify({ version: 1 }),
    });
    return response.status;
  }, draft);
  expect(status).toBe(403);
  await expectNoSecretsInStorage(page);
});
