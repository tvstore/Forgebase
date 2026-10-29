# sales

An example [Forgebase](../../README.md) app in the role of the ERPNext sales module: customers, customer groups and submittable sales orders. It is documentation that runs: it uses the public `forgebase` crate only and changes nothing under `crates/`.

It shows:

- **DocTypes** (`doctypes/*.toml`, embedded with `forgebase::doctypes!`): a Link (`customer` to `customer_group`), a child table (`sales_order.items`), a naming series, a submittable DocType and role permissions (`Sales User`, `Sales Manager`).
- **Hooks** (`src/lib.rs`, `OrderRules` on `sales_order`): `validate` computes each item's `amount` and the order's `total`; `before_submit` refuses an order without items or with a total that is not positive.
- **A data migration** (`migrations/0001_default_customer_groups.sql`): creates the customer groups `Retail` and `Wholesale`, once per database.
- **Tests** (`tests/sales.rs`): hooks without a database, and the HTTP API against a real PostgreSQL.

## Run it

Needs PostgreSQL 16 and an empty database.

```sh
export FORGEBASE_DATABASE_URL=postgres://user:password@localhost:5432/sales
cargo run -p sales -- migrate
echo 'a long password' | cargo run -p sales -- user create ann --role "Sales User"
FORGEBASE_DEV_INSECURE_COOKIES=1 cargo run -p sales -- serve     # http://127.0.0.1:8000/api/v1
```

The commands (`doc`, `user`, `token`, ...) are those of the stock binary; see [the quick start](../../README.md#quick-start) for what to do with them. `FORGEBASE_DEV_INSECURE_COOKIES=1` is for plain-HTTP development only.

## Test it

```sh
export FORGEBASE_TEST_DATABASE_URL=postgres://user:password@localhost:5432/postgres   # a superuser or CREATEDB URL
cargo test -p sales
```

Every database test creates and drops its own database. They fail, they do not skip, when the variable is not set.

## Notes

- The computed `amount` and `total` overwrite what a client sends. Amounts are rounded to 2 places, half away from zero.
- `sales_order.items` is not required, so a draft can be saved without items; submitting is what needs them.
- [`sales_extension`](../sales_extension/README.md) builds on this app.
