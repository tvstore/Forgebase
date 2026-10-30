// The smoke test of the desk (M4.2, extended in UX.2 and UX.4): login -> home -> list -> search -> create
// (with a child row) -> save (toast) -> edit -> submit (badge) -> the More menu on the example
// `sales_order` DocType (submittable, naming series, a child table, a Link to `customer`), then a
// user without the submit right. Home, the list and the form (a draft and a submitted order) are
// also checked for horizontal scroll at 375 and 320 px, so that a runner with a wider fallback font
// than the developer's machine does not surprise us. It runs against a real server prepared by seed.sh and checks the
// M4 exit criteria that need a browser: no secret in browser storage, and an action the user has
// no right for is hidden (the API refusal itself is tested in Rust: forgebase-http/tests/resources.rs
// and crates/forgebase-db/tests/permissions.rs; one refused request is repeated here). UX.4 adds
// the command bar (Ctrl+K), the filter chips, link titles and reduced motion as small tests of
// their own, and the link title in the list and the form to the main flow. M8.2 adds the workflow
// on the e2e-only DocType `purchase_request` (seed.sh): a clerk sends a request for approval, a
// manager approves it; the state badge, the actions, the confirmation, the history and the state
// filter. Sales orders keep their plain Submit.

import { expect, test } from '@playwright/test';
import type { Page } from '@playwright/test';

const PASSWORD = process.env.E2E_PASSWORD ?? 'e2e-test-password';
const CUSTOMER = 'ada@example.com';
// The seeded customer has a `title_field` (its full name): a Link to it shows that title.
const CUSTOMER_TITLE = 'Ada Lovelace';

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

const PHONE_WIDTHS = [375, 320];

/**
 * The open page does not scroll sideways at the width of a phone, and of a small one (only a table
 * may, inside its own container). The viewport is put back afterwards.
 */
async function expectNoHorizontalScroll(page: Page, what: string): Promise<void> {
  const before = page.viewportSize() ?? { width: 1280, height: 800 };
  for (const width of PHONE_WIDTHS) {
    await page.setViewportSize({ width, height: 800 });
    const widths = await page.evaluate(() => ({
      scroll: document.documentElement.scrollWidth,
      client: document.documentElement.clientWidth,
    }));
    expect(widths.scroll, `${what} scrolls sideways at ${width} px`).toBeLessThanOrEqual(widths.client);
  }
  await page.setViewportSize(before);
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
  await expect(page.getByRole('heading', { name: 'New Sales Order', exact: true })).toBeFocused();

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
  await expect(page.getByRole('main').getByRole('link', { name: 'Sales Order', exact: true })).toBeVisible();
  await expectNoHorizontalScroll(page, 'home');
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
  await expectNoHorizontalScroll(page, 'the list');

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
  // The heading is the title field (the customer, shown by its title), with the document's name
  // muted under it.
  await expect(page.getByRole('heading', { name: CUSTOMER_TITLE, level: 1 })).toBeFocused();
  await expect(page.locator('.doc-title-name')).toHaveText(name);
  await expect(statusBadge(page)).toHaveText('Draft');
  await expect(page.getByRole('navigation', { name: 'Breadcrumb' })).toContainText(name);
  await expect(page.getByLabel('Customer')).toHaveValue(CUSTOMER); // the control holds the key
  await expect(page.getByLabel('Rate, row 1')).toHaveValue('9.50');
  await expect(page.getByRole('button', { name: 'Submit' })).toBeVisible();
  await expect(page.getByText('* required')).toBeVisible(); // a draft has fields to fill in
  // Delete is in the More menu (a draft may be deleted by a manager); Escape closes the menu.
  await page.getByRole('button', { name: 'More actions' }).click();
  await expect(page.getByRole('button', { name: 'Delete' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('button', { name: 'Delete' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'More actions' })).toBeFocused();
  await expectNoHorizontalScroll(page, 'the draft form');

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
  // A locked value is plain text, not an input: the label is its term and the value, the link's
  // title with its key muted after it, is its definition.
  await expect(page.getByLabel('Customer')).toHaveCount(0);
  await expect(page.getByRole('term').filter({ hasText: 'Customer' })).toHaveText('Customer');
  const customerValue = page.getByRole('definition').filter({ hasText: CUSTOMER_TITLE });
  await expect(customerValue).toContainText(CUSTOMER);
  await expect(page.getByRole('button', { name: 'Add row' })).toHaveCount(0);
  await expect(page.getByText('* required')).toHaveCount(0); // every required field is a value now
  await expectNoSecretsInStorage(page);

  // The order is in the list, with its status.
  await page.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'Sales Order' }).click();
  // The primary text is the title field (the customer's title), with the order's name muted beside it.
  await expect(page.getByRole('row', { name: new RegExp(`${CUSTOMER_TITLE}.*${name}.*Submitted`) })).toBeVisible();
  await page.getByRole('row', { name: new RegExp(name) }).getByRole('cell').first().click({ position: { x: 4, y: 4 } }); // the whole row opens it
  await expect(page).toHaveURL(new RegExp(`#/form/sales_order/${name}$`));

  // ---- a phone: no horizontal scroll on the list and the form ---------------------------
  await expectNoHorizontalScroll(page, 'the submitted form');
  await page.goto('/desk/#/list/sales_order');
  await expect(page.getByRole('row', { name: new RegExp(name) })).toBeVisible();
  await expectNoHorizontalScroll(page, 'the list with orders');
  // On a phone the status badge is not off-screen in the scrolling table: it sits under the title.
  await page.setViewportSize({ width: 375, height: 800 });
  const inline = page.getByRole('row', { name: new RegExp(name) }).locator('.status-inline');
  await expect(inline).toBeVisible();
  const box = await inline.boundingBox();
  expect((box?.x ?? 0) + (box?.width ?? 0)).toBeLessThanOrEqual(375);

  // The menu drawer: the focus goes to its first link, Escape closes it and returns to the Menu
  // button, and a click outside (on the backdrop) closes it too.
  const menu = page.getByRole('button', { name: 'Menu' });
  const drawer = page.getByRole('navigation', { name: 'Document types' });
  await menu.click();
  await expect(menu).toHaveAttribute('aria-expanded', 'true');
  await expect(drawer.getByRole('link', { name: 'Home' })).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(menu).toHaveAttribute('aria-expanded', 'false');
  await expect(menu).toBeFocused();
  await menu.click();
  await expect(drawer).toBeVisible();
  await page.mouse.click(360, 500);
  await expect(drawer).toBeHidden();
  await expect(menu).toHaveAttribute('aria-expanded', 'false');

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

// ---- UX.4: the command bar --------------------------------------------------------------

test('the command bar opens with Ctrl+K, jumps to a list and a new form, and gives the focus back', async ({ page }) => {
  await login(page, 'e2e_manager');
  const bar = page.getByRole('dialog', { name: 'Search or jump to' });

  // Ctrl+K opens it with the focus in its input; typing a DocType's label and Enter opens its list.
  await page.keyboard.press('Control+k');
  await expect(bar).toBeVisible();
  const input = bar.getByRole('combobox');
  await expect(input).toBeFocused();
  await input.fill('sales ord');
  await expect(bar.getByRole('option', { name: 'Go to Sales Order', exact: true })).toBeVisible();
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/#\/list\/sales_order$/);
  await expect(bar).toHaveCount(0);
  await expect(page.getByRole('heading', { name: 'Sales Order', level: 1 })).toBeVisible();

  // The arrow keys move the active option (aria-activedescendant); Enter opens "New".
  await page.keyboard.press('Control+k');
  await input.fill('sales order');
  const goTo = bar.getByRole('option', { name: 'Go to Sales Order', exact: true });
  const create = bar.getByRole('option', { name: 'New Sales Order', exact: true });
  await expect(goTo).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('ArrowDown');
  await expect(goTo).toHaveAttribute('aria-selected', 'false');
  // ArrowDown walks the list down to "New".
  for (let i = 0; i < 10 && (await create.getAttribute('aria-selected')) !== 'true'; i++) {
    await page.keyboard.press('ArrowDown');
  }
  await expect(create).toHaveAttribute('aria-selected', 'true');
  await expect(input).toHaveAttribute('aria-activedescendant', (await create.getAttribute('id')) ?? '');
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/#\/new\/sales_order$/);

  // Nothing matches: it says so. Escape closes it and the focus returns to the search button.
  const button = page.getByRole('button', { name: /^Search or jump to/ });
  await button.click();
  await expect(bar).toBeVisible();
  await input.fill('zzz-nothing');
  await expect(bar.getByText('No matches.')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(bar).toHaveCount(0);
  await expect(button).toBeFocused();
  await expectNoSecretsInStorage(page); // the recent documents are in memory only
});

test('the recent documents of one user are not shown to the next user of the tab', async ({ page }) => {
  await login(page, 'e2e_manager');
  await page.getByRole('main').getByRole('link', { name: 'Sales Order', exact: true }).click();
  await fillNewOrder(page);
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page).toHaveURL(/#\/form\/sales_order\/SO-\d{4}-\d{5}$/);
  const name = decodeURIComponent(page.url().split('/').pop() ?? '');

  await expect(toast(page, 'Saved')).toBeVisible();
  const bar = page.getByRole('dialog', { name: 'Search or jump to' });
  await page.keyboard.press('Control+k');
  await expect(bar).toBeVisible();
  await expect(bar.getByRole('group', { name: 'Recent' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(bar).toHaveCount(0);

  // Logged out and in again as someone else, in the same tab: nothing of the first user is left.
  await logout(page);
  // Log in again WITHOUT navigating: a page load would wipe the in-memory list by itself and the
  // test would prove nothing. The marker shows that it is still the same document.
  await page.evaluate(() => {
    (window as unknown as { __sameDocument?: boolean }).__sameDocument = true;
  });
  await page.getByLabel('Username').fill('e2e_clerk');
  await page.getByLabel('Password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Log in' }).click();
  await expect(page.getByRole('heading', { name: 'Home', level: 1 })).toBeVisible();
  expect(await page.evaluate(() => (window as unknown as { __sameDocument?: boolean }).__sameDocument)).toBe(true);
  await page.keyboard.press('Control+k');
  await expect(bar).toBeVisible();
  // Wait for /api/v1/meta: without DocTypes the Recent group is filtered out anyway.
  await expect(bar.getByRole('option', { name: 'Go to Sales Order', exact: true })).toBeVisible();
  await expect(bar.getByRole('group', { name: 'Recent' })).toHaveCount(0);
  await expect(bar.getByText(name)).toHaveCount(0);
});

test('a clerk without create rights is not offered "New" in the command bar', async ({ page }) => {
  await login(page, 'e2e_clerk');
  await page.keyboard.press('Control+k');
  const bar = page.getByRole('dialog', { name: 'Search or jump to' });
  await bar.getByRole('combobox').fill('customer');
  await expect(bar.getByRole('option', { name: 'Go to Customer', exact: true })).toBeVisible();
  await expect(bar.getByRole('option', { name: 'New Customer', exact: true })).toHaveCount(0); // the clerk may only read customers
});

// ---- UX.4: filter chips and link titles -------------------------------------------------

test('a filter shows as a chip that can be removed; a Link shows its title', async ({ page }) => {
  await login(page, 'e2e_manager');
  await page.goto('/desk/#/list/sales_order');
  await expectLoaded(page);

  // The seeded order's customer link shows its title, not the key.
  const orders = page.getByRole('row').filter({ hasText: CUSTOMER_TITLE });
  await expect(orders.first()).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: CUSTOMER })).toHaveCount(0);

  await page.getByRole('button', { name: 'Filter', exact: true }).click();
  await page.getByRole('button', { name: 'Add filter' }).click();
  await page.getByLabel('Filter 1 field').selectOption({ label: 'Customer' });
  await page.getByLabel('Filter 1 value').fill(CUSTOMER);
  // Applied a moment after the last change: the chip appears and the address holds the filter.
  const chip = page.getByRole('button', { name: `Remove filter: Customer is ${CUSTOMER}` });
  await expect(chip).toBeVisible();
  await expect(page).toHaveURL(/[?&]filters=/);
  await expect(orders.first()).toBeVisible(); // the orders of that customer are still listed

  await chip.click();
  await expect(chip).toHaveCount(0);
  await expect(page).not.toHaveURL(/filters=/);
  await expect(page.getByLabel('Filter 1 field')).toHaveCount(0); // the removed row left the filter controls
  await expectLoaded(page);
});

// ---- UX.4: reduced motion ---------------------------------------------------------------

test('prefers-reduced-motion switches the motion of a dialog off', async ({ page }) => {
  await login(page, 'e2e_manager');
  await page.goto('/desk/#/new/sales_order');
  await page.getByLabel('Remarks').fill('unsaved');

  const durations = async () => {
    await page.getByRole('link', { name: 'Home' }).click(); // asks about the unsaved changes
    const dialog = page.getByRole('dialog', { name: 'You have unsaved changes.' });
    await expect(dialog).toBeVisible();
    const result = await dialog.evaluate((el) => {
      const style = getComputedStyle(el);
      return {
        animation: style.animationDuration,
        transition: style.transitionDuration,
        backdrop: getComputedStyle(el, '::backdrop').animationDuration,
      };
    });
    await page.getByRole('button', { name: 'Stay' }).click();
    await expect(dialog).toHaveCount(0);
    return result;
  };

  // With motion, the dialog fades and scales in...
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  const moving = await durations();
  expect(moving.animation).not.toBe('0s');
  expect(moving.backdrop).not.toBe('0s');
  // ...and with reduced motion, every animation and transition has no duration.
  await page.emulateMedia({ reducedMotion: 'reduce' });
  expect(await durations()).toEqual({ animation: '0s', transition: '0s', backdrop: '0s' });
});

// ---- M8.2: the workflow -----------------------------------------------------------------

/** The entries of the Activity card, newest first. */
const history = (page: Page) => page.getByRole('list', { name: 'Workflow history' }).getByRole('listitem');

/** From the Purchase Request list: a new request with a title and an amount, saved. Returns its name. */
async function createRequest(page: Page, title: string): Promise<string> {
  await page.goto('/desk/#/new/purchase_request');
  await expect(page.getByRole('heading', { name: 'New Purchase Request', exact: true })).toBeFocused();
  await page.getByLabel('Title').fill(title);
  await page.getByLabel('Amount').fill('120.50');
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page).toHaveURL(/#\/form\/purchase_request\/PR-\d{4}-\d{5}$/);
  await expect(toast(page, 'Saved')).toBeVisible();
  return decodeURIComponent(page.url().split('/').pop() ?? '');
}

test('a clerk sends a purchase request for approval and a manager approves it', async ({ page }) => {
  // ---- the clerk -------------------------------------------------------------------------
  await login(page, 'e2e_clerk');
  const name = await createRequest(page, 'Office chairs');
  await expect(statusBadge(page)).toHaveText('Draft');
  const sendButton = page.getByRole('button', { name: 'Submit for Approval' });
  await expect(sendButton).toBeVisible();
  // The plain Submit and Cancel are replaced by the workflow's actions; the clerk has no Approve.
  await expect(page.getByRole('button', { name: 'Submit', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Approve' })).toHaveCount(0);
  await expect(page.getByRole('heading', { name: 'Activity', level: 2 })).toBeVisible();
  await expect(page.getByText('No transitions yet.')).toBeVisible();
  await expectNoHorizontalScroll(page, 'the workflow form');

  // Unsaved changes: the action is disabled (Save comes first), and clicking it says why.
  await page.getByLabel('Amount').fill('130.00');
  await expect(sendButton).toBeDisabled();
  await expect(sendButton).toHaveAttribute('title', 'Save first');
  await expect(page.getByRole('button', { name: 'Save' })).toHaveClass(/btn-primary/);
  await sendButton.click({ force: true });
  await expect(page.getByRole('alert')).toContainText('Save them first');
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(toast(page, 'Saved').last()).toBeVisible();
  await expect(sendButton).toBeEnabled();
  await expect(sendButton).toHaveClass(/btn-primary/);

  // The confirmation names the move; Back (and Escape) answers no and nothing changes.
  await sendButton.click();
  const dialog = page.getByRole('dialog', { name: 'Submit for Approval' });
  await expect(dialog).toContainText(`Move Office chairs from Draft to Pending Approval?`);
  await expect(dialog.getByRole('button', { name: 'Back' })).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  await expect(sendButton).toBeFocused();
  await expect(statusBadge(page)).toHaveText('Draft');
  await sendButton.click();
  await dialog.getByRole('button', { name: 'Back' }).click();
  await expect(dialog).toHaveCount(0);

  // Confirm: the version is sent with the action. The new actions are delayed, so the old ones
  // (fetched for the old version) are visible for a while: they must be disabled, not usable.
  await page.route('**/transitions', async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 2000));
    await route.continue();
  });
  await sendButton.click();
  const request = page.waitForRequest((r) => r.method() === 'POST' && r.url().endsWith('/transition'));
  await dialog.getByRole('button', { name: 'Confirm' }).click();
  expect(JSON.parse((await request).postData() ?? '{}')).toEqual({ action: 'Submit for Approval', version: 2 });
  await expect(toast(page, 'Submit for Approval: Pending Approval')).toBeVisible();
  await expect(statusBadge(page)).toHaveText('Pending Approval');
  await expect(sendButton).toBeDisabled(); // still the old action: the new list is in flight
  await sendButton.click({ force: true });
  await expect(page.getByRole('dialog')).toHaveCount(0);
  await expect(sendButton).toHaveCount(0);
  await page.unroute('**/transitions');
  await expect(page.getByRole('button', { name: 'Approve' })).toHaveCount(0); // not the clerk's action
  await expect(history(page)).toHaveCount(1);
  await expect(history(page).first()).toContainText('e2e_clerk Submit for Approval');
  await expect(history(page).first()).toContainText('Draft → Pending Approval');
  await expect(history(page).first().locator('time')).toHaveAttribute('datetime', /^\d{4}-/);
  await expectNoSecretsInStorage(page);
  await logout(page);

  // ---- the manager -----------------------------------------------------------------------
  await login(page, 'e2e_manager');
  await page.goto(`/desk/#/form/purchase_request/${encodeURIComponent(name)}`);
  await expect(statusBadge(page)).toHaveText('Pending Approval');
  const approve = page.getByRole('button', { name: 'Approve' });
  await expect(approve).toHaveClass(/btn-primary/); // the first action is the primary one
  await expect(page.getByRole('button', { name: 'Reject' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Submit', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0);
  await expect(history(page)).toHaveCount(1);
  await expectNoHorizontalScroll(page, 'the pending form');

  await approve.click();
  await expect(page.getByRole('dialog', { name: 'Approve' })).toContainText(
    'Move Office chairs from Pending Approval to Approved?',
  );
  await page.getByRole('button', { name: 'Confirm' }).click();
  await expect(toast(page, 'Approve: Approved')).toBeVisible();
  await expect(statusBadge(page)).toHaveText('Approved');
  await expect(history(page)).toHaveCount(2);
  await expect(history(page).first()).toContainText('e2e_manager Approve'); // newest first
  await expect(history(page).last()).toContainText('e2e_clerk Submit for Approval');
  await expect(page.getByRole('button', { name: 'Approve' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Cancel Approval' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0); // the plain Cancel stays hidden
  await expect(page.getByRole('heading', { name: 'Office chairs', level: 1 })).toBeFocused();
  await expectNoHorizontalScroll(page, 'the approved form');

  // ---- the list: the state is the badge, and it filters ------------------------------------
  await page.goto('/desk/#/list/purchase_request');
  await expectLoaded(page);
  await expect(page.getByRole('columnheader', { name: 'Status' })).toBeVisible();
  const row = page.getByRole('row', { name: new RegExp(`Office chairs.*${name}.*Approved`) });
  await expect(row).toBeVisible();
  await page.getByRole('button', { name: 'Filter', exact: true }).click();
  await page.getByRole('button', { name: 'Add filter' }).click();
  await page.getByLabel('Filter 1 field').selectOption({ label: 'Status' });
  await page.getByLabel('Filter 1 value').selectOption('Draft');
  await expect(page.getByRole('button', { name: 'Remove filter: Status is Draft' })).toBeVisible();
  await expect(row).toHaveCount(0);
  await page.getByLabel('Filter 1 value').selectOption('Approved');
  await expect(page.getByRole('button', { name: 'Remove filter: Status is Approved' })).toBeVisible();
  await expect(row).toBeVisible();
  await expectNoHorizontalScroll(page, 'the workflow list');
});

test('a workflow action on a stale version shows the conflict and Reload brings the actions back', async ({ page }) => {
  await login(page, 'e2e_manager');
  const name = await createRequest(page, 'Stale request');
  // Somebody else changes the document after it was opened.
  const status = await page.evaluate(async (documentName) => {
    const session = (await (await fetch('/api/v1/session', { headers: { 'X-Forgebase-CSRF': 'refresh' } })).json()) as {
      data: { csrf_token: string };
    };
    const response = await fetch(`/api/v1/resource/purchase_request/${encodeURIComponent(documentName)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json', 'X-Forgebase-CSRF': session.data.csrf_token },
      body: JSON.stringify({ version: 1, data: { amount: '1.00' } }),
    });
    return response.status;
  }, name);
  expect(status).toBe(200);

  await page.getByRole('button', { name: 'Submit for Approval' }).click();
  await page.getByRole('button', { name: 'Confirm' }).click();
  await expect(page.getByRole('alert')).toContainText('Someone else changed this document');
  await expect(statusBadge(page)).toHaveText('Draft');
  await page.getByRole('button', { name: 'Reload' }).click();
  await expect(toast(page, 'Loaded the current version')).toBeVisible();
  await expect(page.getByLabel('Amount')).toHaveValue('1.00');
  await page.getByRole('button', { name: 'Submit for Approval' }).click();
  await page.getByRole('button', { name: 'Confirm' }).click();
  await expect(statusBadge(page)).toHaveText('Pending Approval');
});
