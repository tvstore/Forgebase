# HTTP API

`serve` (here `cargo run -p sales -- serve`) runs the API on `FORGEBASE_LISTEN` (default `127.0.0.1:8000`). It serves the same DocTypes as `migrate` (the app's, or the `--doctypes` directory in the stock binary) but **does not run migrations**: run `migrate` first. Requests need a `Host` header from `FORGEBASE_ALLOWED_HOSTS` (comma separated host names without ports, default `localhost,127.0.0.1`; an entry with a port is refused at startup). Ctrl-C or SIGTERM stops accepting connections and lets running requests finish for up to 10 seconds; a second signal, or the end of that period, exits at once (exit code 0; open database transactions are rolled back by PostgreSQL when the connection closes). The session cookie is `Secure`, so browsers send it over HTTPS only: put a TLS-terminating reverse proxy in front (the application has no TLS). For local development over plain HTTP, `FORGEBASE_DEV_INSECURE_COOKIES=1` omits `Secure` (and prints a warning); never use it in production. There is no CORS: serve the front-end from the same origin.

```sh
# Prepare: tables, a user with the Administrator role (the example DocTypes name other roles; Administrator may do everything)
cargo run -p sales -- migrate
printf '%s\n' 'a long password' | cargo run -p sales -- user create root --role Administrator

# Terminal 1: serve (development mode)
FORGEBASE_DEV_INSECURE_COOKIES=1 cargo run -p sales -- serve

# Terminal 2: log in. The response has the one-time CSRF token; the session cookie is saved in jar.txt.
CSRF=$(curl -s -c jar.txt -H 'Content-Type: application/json' \
  -d '{"username": "root", "password": "a long password"}' \
  http://localhost:8000/api/v1/login | sed -n 's/.*"csrf_token":"\([0-9a-f]*\)".*/\1/p')

# Create (201). Changing requests with the cookie need the CSRF header; reads do not.
curl -s -b jar.txt -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"email": "ann@example.com", "full_name": "Ann", "tier": "gold", "loyalty_points": 12}' \
  http://localhost:8000/api/v1/resource/customer

# List with filters (a JSON array like `doc list --filter`), sort, limit and offset
curl -s -b jar.txt -G http://localhost:8000/api/v1/resource/customer \
  --data-urlencode 'filters=[{"field":"tier","op":"=","value":"gold"}]' \
  --data-urlencode 'sort=loyalty_points:desc' --data-urlencode 'limit=10'

# Read, then change with the version you read (PATCH needs {"version", "data"}; a stale version is 409 `conflict`)
curl -s -b jar.txt http://localhost:8000/api/v1/resource/customer/ann@example.com
curl -s -b jar.txt -X PATCH -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"version": 1, "data": {"loyalty_points": 20}}' \
  http://localhost:8000/api/v1/resource/customer/ann@example.com

# Create a sales order (its name comes from the series; use the `name` in the response below), then send it for approval and approve it
# (a transition takes {"action", "version"}; the order has a workflow, so there is no plain submit. On a DocType without a workflow, submit and cancel take {"version": N}; amend takes {"version": N} and answers 201 with the new draft.
# The user needs a role of the transition and the right: `root` is an Administrator, who passes every role check)
curl -s -b jar.txt -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"customer": "ann@example.com", "items": [{"description": "Widget", "qty": 2, "rate": "9.50"}]}' \
  http://localhost:8000/api/v1/resource/sales_order
curl -s -b jar.txt -X POST -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"action": "Submit for Approval", "version": 1}' http://localhost:8000/api/v1/resource/sales_order/SO-2026-00001/transition
curl -s -b jar.txt -X POST -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"action": "Approve", "version": 2}' http://localhost:8000/api/v1/resource/sales_order/SO-2026-00001/transition

# Delete (204; the version goes in the query string). Here it is refused with 409 `linked_document_exists`, because the sales order links to the customer
curl -s -b jar.txt -X DELETE -H "X-Forgebase-CSRF: $CSRF" \
  'http://localhost:8000/api/v1/resource/customer/ann@example.com?version=2'

# Log out (204; clears the cookie and ends the session)
curl -s -b jar.txt -X POST -H "X-Forgebase-CSRF: $CSRF" http://localhost:8000/api/v1/logout

# A machine client uses an API token instead: no cookie, no CSRF header. The token header wins over any cookie.
TOKEN=$(cargo run -q -p sales -- token create root)    # prints key:secret once
curl -s -H "Authorization: token $TOKEN" http://localhost:8000/api/v1/resource/customer
```

Responses are JSON: `{"data": ...}` on success and `{"error": {"code", "message", "details"?}}` on failure, with stable codes (for example `unauthenticated` 401, `permission_denied` 403, `csrf_failed` 403, `not_found` 404, `conflict` 409, `validation_failed` 422, `invalid_query` 400). A document you cannot read is a 404, like a missing one. The route table, the request pipeline, the codes and the limits are in the [architecture](../blueprint/architecture.md#7-api-rest-resource-api-implemented-in-m33-metadata-and-session-routes-in-m41-the-rest-proposed) and the [security model](../blueprint/security-model.md#the-http-layer-implemented-in-m33).

## App methods

An app can register its own endpoints ([first app, section 8](first-app.md#8-app-methods), [ADR 0019](../adr/0019-app-method-endpoints.md)): `POST /api/v1/method/<app>.<method>` with a JSON object body (an empty body is `{}`) answers `200 {"data": ..}`. The caller must be logged in (a cookie with the CSRF header, or an API token) and have a role of the method, or the method must be declared `any_user`; **there are no guest methods**. The call runs as that user in one transaction. The `sales` example has no method yet (M9.5 adds one); this is a call to the `sales.orders_of` method of the first-app guide, after the login of the first example above:

```sh
curl -s -b jar.txt -H "X-Forgebase-CSRF: $CSRF" -H 'Content-Type: application/json' \
  -d '{"customer": "ann@example.com"}' http://localhost:8000/api/v1/method/sales.orders_of
# a token call: no cookie and no CSRF header
curl -s -H "Authorization: token $TOKEN" -H 'Content-Type: application/json' \
  -d '{"customer": "ann@example.com"}' http://localhost:8000/api/v1/method/sales.orders_of
```

| Status | When |
|---|---|
| 200 | `{"data": <what the method returned>}` |
| 400 | the body is not a JSON object (`bad_request`), a query parameter was sent (`invalid_query`), or the method refused (`method_error`, with the app's message) |
| 401, 403 `csrf_failed` | no valid credential, or a cookie call without the CSRF header |
| 404 | the method does not exist, its app is not installed on the site, **or you may not call it**: one answer, so names are not revealed |
| 405 | any HTTP method but `POST` |
| 409, 403 `permission_denied`, 422 | an error of a document the method touched, with the status and code of the document API; 422 `method_error` is the method's own refusal |
| 413, 415 | the body is over the limit, or not `application/json` when it is not empty |
| 500 | a failure inside the method or the server: a generic message, the cause is in the server log |

Whatever the method did in the transaction is rolled back on every error.

