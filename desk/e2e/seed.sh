#!/usr/bin/env bash
# Prepares a database for the Playwright smoke test (desk/e2e/smoke.spec.ts). Run it from the
# repository root with an EMPTY (scratch) database in FORGEBASE_DATABASE_URL; the URL is read by
# the forgebase tool from the environment and is never printed here.
#
#   FORGEBASE_DATABASE_URL=postgres://... desk/e2e/seed.sh
#
# It writes the DocTypes the test serves to $E2E_DIR/doctypes (default target/e2e): the DocTypes of
# the example app examples/sales plus a role "Sales Clerk" that may read, write and create sales
# orders but not submit them (no example role is like that). That e2e-only role is why the test
# uses the stock binary with --doctypes, and not the binary of the sales app (whose DocTypes are
# embedded and cannot take the extra role). The sales app's hooks do not run in this
# test: `amount` and `total` (read_only, computed by those hooks) stay empty here. It then migrates, and creates:
#   - e2e_manager, role "Sales Manager": every right on sales orders (and customers);
#   - e2e_clerk, role "Sales Clerk": no submit, cancel, amend or delete right;
#   - the customer "ada@example.com" for the orders.
# Passwords come from E2E_PASSWORD (default below): a throw-away password of a throw-away database.
# Start the server with:  forgebase serve --doctypes "$E2E_DIR/doctypes"
set -euo pipefail

: "${FORGEBASE_DATABASE_URL:?set FORGEBASE_DATABASE_URL to an empty scratch database}"
BIN="${FORGEBASE_BIN:-target/debug/forgebase}"
E2E_DIR="${E2E_DIR:-target/e2e}"
PASSWORD="${E2E_PASSWORD:-e2e-test-password}"
DOCTYPES="$E2E_DIR/doctypes"

rm -rf "$DOCTYPES"
mkdir -p "$DOCTYPES"
cp examples/sales/doctypes/*.toml "$DOCTYPES/"
cat >> "$DOCTYPES/sales_order.toml" <<'TOML'

# e2e only: may work on orders but not submit them.
[[permissions]]
role = "Sales Clerk"
read = true
write = true
create = true
TOML
cat >> "$DOCTYPES/customer.toml" <<'TOML'

# e2e only: needed to read the Link target of a sales order.
[[permissions]]
role = "Sales Clerk"
read = true
TOML

"$BIN" migrate --doctypes "$DOCTYPES" > /dev/null
printf '%s\n' "$PASSWORD" | "$BIN" user create e2e_manager --role "Sales Manager" > /dev/null
printf '%s\n' "$PASSWORD" | "$BIN" user create e2e_clerk --role "Sales Clerk" > /dev/null
"$BIN" doc insert customer --doctypes "$DOCTYPES" \
  --data '{"email":"ada@example.com","full_name":"Ada Lovelace"}' > /dev/null
echo "seeded: users e2e_manager and e2e_clerk, customer ada@example.com, DocTypes in $DOCTYPES"
