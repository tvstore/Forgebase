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
# uses --doctypes, and not the binary of the sales app (whose DocTypes are embedded and cannot take
# the extra role). The binary is the example `e2e_app` of the forgebase crate (the stock command plus
# one app that defines the query report "Sales by customer", M8.4: a report can only come from an app
# compiled into the binary; build it with `cargo build -p forgebase --example e2e_app`). The sales
# app's hooks do not run in this test: `amount` and `total` (read_only, computed by those hooks) stay empty here. It then migrates, and creates:
#   - e2e_manager, role "Sales Manager": every right on sales orders (and customers);
#   - e2e_clerk, role "Sales Clerk": no submit, cancel, amend or delete right;
#   - e2e_admin, role "Administrator": the only user who may share a saved view (M8.3);
#   - the customer "ada@example.com" (full name "Ada Lovelace", the customer's `title_field`) for the orders:
#     a Link to it shows that title in the list and the form (UX.4, link titles).
#   - a separate DocType `purchase_request` with a workflow (M8.2), so that the sales order flow keeps its plain
#     Submit: Draft -> Pending Approval (Sales Clerk or Sales Manager), Pending Approval -> Approved (Sales Manager,
#     submits), Pending Approval -> Draft "Reject" (Sales Manager), Approved -> Cancelled (Sales Manager, cancels).
# Passwords come from E2E_PASSWORD (default below): a throw-away password of a throw-away database.
# Start the server with:  target/debug/examples/e2e_app serve --doctypes "$E2E_DIR/doctypes"
set -euo pipefail

: "${FORGEBASE_DATABASE_URL:?set FORGEBASE_DATABASE_URL to an empty scratch database}"
BIN="${FORGEBASE_BIN:-target/debug/examples/e2e_app}"
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

# e2e only: a DocType with a workflow. The clerk may write but not submit; the transition roles narrow
# what the rights allow (ADR 0015): "Approve" needs the role Sales Manager and the submit right.
cat > "$DOCTYPES/purchase_request.toml" <<'TOML'
name = "purchase_request"
label = "Purchase Request"
submittable = true
naming = { series = "PR-.YYYY.-.#####" }
title_field = "title"
icon = "clipboard-list"
list_fields = ["title", "amount"]

[[fields]]
name = "title"
type = "data"
required = true

[[fields]]
name = "amount"
type = "decimal"
precision = 12
scale = 2

[[permissions]]
role = "Sales Clerk"
read = true
write = true
create = true

[[permissions]]
role = "Sales Manager"
read = true
write = true
create = true
delete = true
submit = true
cancel = true
amend = true

[workflow]

[[workflow.states]]
name = "Draft"
docstatus = 0
style = "neutral"

[[workflow.states]]
name = "Pending Approval"
docstatus = 0
style = "warning"

[[workflow.states]]
name = "Approved"
docstatus = 1
style = "success"

[[workflow.states]]
name = "Cancelled"
docstatus = 2
style = "danger"

[[workflow.transitions]]
action = "Submit for Approval"
from = "Draft"
to = "Pending Approval"
roles = ["Sales Clerk", "Sales Manager"]

[[workflow.transitions]]
action = "Approve"
from = "Pending Approval"
to = "Approved"
roles = ["Sales Manager"]

[[workflow.transitions]]
action = "Reject"
from = "Pending Approval"
to = "Draft"
roles = ["Sales Manager"]

[[workflow.transitions]]
action = "Cancel Approval"
from = "Approved"
to = "Cancelled"
roles = ["Sales Manager"]
TOML

"$BIN" migrate --doctypes "$DOCTYPES" > /dev/null
printf '%s\n' "$PASSWORD" | "$BIN" user create e2e_manager --role "Sales Manager" > /dev/null
printf '%s\n' "$PASSWORD" | "$BIN" user create e2e_clerk --role "Sales Clerk" > /dev/null
printf '%s\n' "$PASSWORD" | "$BIN" user create e2e_admin --role "Administrator" > /dev/null
"$BIN" doc insert customer --doctypes "$DOCTYPES" \
  --data '{"email":"ada@example.com","full_name":"Ada Lovelace"}' > /dev/null
echo "seeded: users e2e_manager, e2e_clerk and e2e_admin, customer ada@example.com, DocTypes in $DOCTYPES"
