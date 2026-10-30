// The smoke test of the desk (M4.2, extended in UX.2): login -> home -> list -> search -> create
// (with a child row) -> save (toast) -> edit -> submit (badge) -> the More menu on the example
// `sales_order` DocType (submittable, naming series, a child table, a Link to `customer`), then a
// user without the submit right. The list and the form are also checked for horizontal scroll at
// a phone width (375 px). It runs against a real server prepared by seed.sh and checks the
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
  await expect(page.getByRole('heading', { name: 'Home', level: 1 })).toBeVisible();
}

async function logout(page: Page): Promise<void> {
  await page.getByRole('button', { name: /^Account:/ }).click();
  await page.getByRole('button', { name: 'Log out' }).click();
  await expect(page.getByRole('button', { name: 'Log in' })).toBeVisible();
}

/** The page does not scroll sideways (only a table may, inside its own container). */
async function expectNoHorizontalScroll(page: Page): Promise<void> {
  const widths = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    client: document.documentElement.clientWidth,
  }));
  expect(widths.scroll).toBeLessThanOrEqual(widths.client);
}

/** Waits until the list has loaded (nothing in the page is busy any more). */
async function expectLoaded(page: Page): Promise<void> {
  await expect(page.locator('[aria-busy="true"]')).toHaveCount(0);
}

/** The text of the document status badge of the open form. */
const statusBadge = (page: Page) => page.locator('.status-badge');
const toast = (page: Page, text: string) => page.locator('.toast', { hasText: text });

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
  await page.getByRole('main').getByRole('link', { name: 'New', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'New Sales Order' })).toBeFocused();

  // The Link field suggests the names of the target DocType as you type.
  await page.getByLabel('Customer').fill('ada');
  await expect(page.locator(`datalist option[value="${CUSTOMER}"]`)).toHaveCount(1);
  await page.getByLabel('Customer').fill(CUSTOMER);
  await page.getByLabel('Order Date').fill('2026-10-29');

  await page.getByRole('button', { name: 'Add row' }).click();
  await expect(page.getByLabel('Description, row 1')).toBeFocused();
  await page.getByLabel('Description, row 1').fill('Widget');
  await page.getByLabel('Quantity, row 1').fill('2');
  await page.getByLabel('Rate, row 1').fill('9.50');
}

test('login, list, create, edit and submit a sales order; a clerk cannot submit', async ({ page }) => {
  // ---- login, home and list -------------------------------------------------------------
  await login(page, 'e2e_manager');
  await expectNoSecretsInStorage(page);
  await expect(page.getByRole('navigation', { name: 'Document types' })).toBeVisible(); // the sidebar
  await page.getByRole('main').getByRole('link', { name: 'Sales Order', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Sales Order', level: 1 })).toBeVisible();
  await expectLoaded(page); // the list may hold orders of an earlier run

  // ---- search: kept in the address, survives a reload, and clears -----------------------
  const search = page.getByRole('searchbox');
  await search.fill('no-such-order-zzz');
  await expect(page).toHaveURL(/[?&]q=no-such-order-zzz/);
  await expect(page.getByRole('heading', { name: 'No documents match' })).toBeVisible();
  await page.reload();
  await expect(search).toHaveValue('no-such-order-zzz');
  await expect(page.getByRole('heading', { name: 'No documents match' })).toBeVisible();
  await page.getByRole('button', { name: 'Clear search and filters' }).click();
  await expect(search).toHaveValue('');
  await expect(page.getByRole('heading', { name: 'No documents match' })).toHaveCount(0);
  await expectLoaded(page);
  await expectNoHorizontalScroll(page);

  // ---- create ---------------------------------------------------------------------------
  await fillNewOrder(page);

  // A row with a non-integer quantity is refused on its cell, and nothing is created.
  await page.getByLabel('Quantity, row 1').fill('1.5');
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByRole('alert')).toContainText('not saved');
  await expect(page.getByLabel('Quantity, row 1')).toBeFocused();
  await expect(page.getByLabel('Quantity, row 1')).toHaveAttribute('aria-invalid', 'true');
  await expect(page.getByText('Enter a whole number.')).toBeVisible();
  await page.getByLabel('Quantity, row 1').fill('2');

  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page).toHaveURL(/#\/form\/sales_order\/SO-\d{4}-\d{5}$/);
  const name = decodeURIComponent(page.url().split('/').pop() ?? '');
  await expect(toast(page, 'Saved')).toBeVisible(); // success feedback
  await expect(page.getByRole('heading', { name, level: 1 })).toBeFocused();
  await expect(statusBadge(page)).toHaveText('Draft');
  await expect(page.getByRole('navigation', { name: 'Breadcrumb' })).toContainText(name);
  await expect(page.getByLabel('Customer')).toHaveValue(CUSTOMER);
  await expect(page.getByLabel('Rate, row 1')).toHaveValue('9.50');
  await expect(page.getByRole('button', { name: 'Submit' })).toBeVisible();
  // Delete is in the More menu (a draft may be deleted by a manager); Escape closes the menu.
  await page.getByRole('button', { name: 'More actions' }).click();
  await expect(page.getByRole('button', { name: 'Delete' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('button', { name: 'Delete' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'More actions' })).toBeFocused();
  await expectNoHorizontalScroll(page);

  // ---- edit: only the changed field is sent ---------------------------------------------
  await page.getByLabel('Remarks').fill('first remark');
  const patch = page.waitForRequest((r) => r.method() === 'PATCH');
  await page.getByRole('button', { name: 'Save' }).click();
  expect(JSON.parse((await patch).postData() ?? '{}')).toEqual({ version: 1, data: { remarks: 'first remark' } });
  await expect(toast(page, 'Saved').last()).toBeVisible();
  await expect(page.locator('dd', { hasText: /^2$/ })).toBeVisible();

  // Unsaved changes are not lost by navigating away: the page asks first.
  await page.getByLabel('Remarks').fill('unsaved remark');
  await page.getByRole('link', { name: 'Home' }).click();
  await expect(page.getByRole('dialog', { name: 'You have unsaved changes.' })).toBeVisible();
  await page.getByRole('button', { name: 'Stay' }).click();
  await expect(page).toHaveURL(/#\/form\/sales_order\//);
  await expect(page.getByLabel('Remarks')).toHaveValue('unsaved remark');
  await page.getByLabel('Remarks').fill('first remark');

  // ---- Ctrl+S saves ---------------------------------------------------------------------
  await page.getByLabel('Remarks').fill('saved with the keyboard');
  const shortcut = page.waitForRequest((r) => r.method() === 'PATCH');
  await page.keyboard.press('Control+s');
  expect(JSON.parse((await shortcut).postData() ?? '{}').data).toEqual({ remarks: 'saved with the keyboard' });
  await expect(page.locator('dd', { hasText: /^3$/ })).toBeVisible();

  // ---- submit ---------------------------------------------------------------------------
  await page.getByRole('button', { name: 'Submit' }).click();
  await expect(toast(page, 'Submitted')).toBeVisible();
  await expect(statusBadge(page)).toHaveText('Submitted');
  await expect(page.getByRole('button', { name: 'Submit' })).toHaveCount(0);
  // Cancel is in the More menu and asks first; Escape answers no and the focus returns.
  await page.getByRole('button', { name: 'More actions' }).click();
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Delete' })).toHaveCount(0); // a submitted document cannot be deleted
  await page.getByRole('button', { name: 'Cancel', exact: true }).click();
  await expect(page.getByRole('dialog', { name: `Cancel ${name}?` })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'More actions' })).toBeFocused();
  await expect(statusBadge(page)).toHaveText('Submitted');
  // A submitted order edits only its allow_on_submit field; its table is read-only.
  await expect(page.getByLabel('Remarks')).toBeEditable();
  await expect(page.getByLabel('Customer')).not.toBeEditable();
  await expect(page.getByRole('button', { name: 'Add row' })).toHaveCount(0);
  await expectNoSecretsInStorage(page);

  // The order is in the list, with its status.
  await page.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'Sales Order' }).click();
  // The primary text is the title field (the customer), with the order's name muted beside it.
  await expect(page.getByRole('row', { name: new RegExp(`${CUSTOMER}.*${name}.*Submitted`) })).toBeVisible();
  await page.getByRole('row', { name: new RegExp(name) }).getByRole('cell').first().click({ position: { x: 4, y: 4 } }); // the whole row opens it
  await expect(page).toHaveURL(new RegExp(`#/form/sales_order/${name}$`));

  // ---- a phone: no horizontal scroll on the list and the form ---------------------------
  await page.setViewportSize({ width: 375, height: 800 });
  await expectNoHorizontalScroll(page); // the form
  await page.goto('/desk/#/list/sales_order');
  await expect(page.getByRole('row', { name: new RegExp(name) })).toBeVisible();
  await expectNoHorizontalScroll(page); // the list
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(`/desk/#/form/sales_order/${encodeURIComponent(name)}`);
  await expect(statusBadge(page)).toHaveText('Submitted');
  await logout(page);

  // ---- a user without the submit right --------------------------------------------------
  await login(page, 'e2e_clerk');
  await page.getByRole('main').getByRole('link', { name: 'Sales Order', exact: true }).click();
  await page.getByRole('row', { name: new RegExp(name) }).getByRole('link').click();
  await expect(statusBadge(page)).toHaveText('Submitted');
  await expect(page.getByRole('button', { name: 'More actions' })).toHaveCount(0); // no cancel, amend or delete right
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Amend' })).toHaveCount(0);

  await page.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'Sales Order' }).click();
  await fillNewOrder(page);
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page).toHaveURL(/#\/form\/sales_order\/SO-\d{4}-\d{5}$/);
  const draft = decodeURIComponent(page.url().split('/').pop() ?? '');
  await expect(statusBadge(page)).toHaveText('Draft');
  await expect(page.getByRole('button', { name: 'Save' })).toBeVisible(); // the clerk can work on the draft
  await expect(page.getByRole('button', { name: 'Submit' })).toHaveCount(0); // but not submit it
  await expect(page.getByRole('button', { name: 'More actions' })).toHaveCount(0); // and not delete it

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
