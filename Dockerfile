# Forgebase image: the built desk plus one application binary (default: the `sales` example),
# PostgreSQL 16 client tools for `site backup` / `site restore`, running as a non-root user.
#
#   docker build -t forgebase-sales .                    # the `sales` example (demo)
#   docker build --build-arg BIN=myapp -t myapp .         # a workspace binary of your own
#
# Guide: docs/guide/deploy.md. Nothing in this file is specific to one CPU architecture: every base
# image below is multi-arch (amd64 and arm64), so build on the machine that will run it.
#
# Base images are pinned by digest (the multi-arch index digest). To update one:
#   docker pull <image>:<tag>
#   docker inspect --format '{{index .RepoDigests 0}}' <image>:<tag>
# then replace the `@sha256:...` below (keep the tag, it says what the digest is) and rebuild.
# See docs/blueprint/maintenance-policy.md ("Container images").

ARG BIN=sales

# ---- Stage 1: the desk (React app) --------------------------------------------------------------
FROM node:22-bookworm-slim@sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da AS desk
WORKDIR /desk
# desk/.npmrc has ignore-scripts=true: dependency install scripts do not run.
COPY desk/package.json desk/package-lock.json desk/.npmrc ./
RUN npm ci
COPY desk/ ./
# Writes dist/, including THIRD-PARTY-LICENSES.txt (it is in desk/public).
RUN npm run build && test -f dist/index.html && test -f dist/THIRD-PARTY-LICENSES.txt

# ---- Stage 2: the application binary ------------------------------------------------------------
# The toolchain of rust-toolchain.toml. RUSTUP_TOOLCHAIN keeps rustup from downloading the clippy and
# rustfmt components that the file asks for (not needed to build); the check below fails the build
# if this image's compiler and the file's channel ever differ.
FROM rust:1.99.1-slim-bookworm@sha256:24707e70a4f1e23d9a7b25202ec43126d28a52294b96aad5c670df1fd1f1d400 AS build
ARG BIN
ENV RUSTUP_TOOLCHAIN=1.99.1
WORKDIR /src
COPY . .
RUN test "$(rustc --version | cut -d' ' -f2)" = "$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)"
RUN cargo build --release --locked -p "${BIN}" \
    && cp "target/release/${BIN}" /forgebase-app

# ---- Stage 3: runtime ---------------------------------------------------------------------------
# postgres:16-bookworm is the base only for pg_dump and pg_restore 16, which must match the server's
# major version (crates/forgebase/src/backup.rs). It needs no apt repository. The cost: the image is
# about 650 MB (a Debian slim base plus the PGDG client package would be about 200 MB), and the base
# declares `VOLUME /var/lib/postgresql/data`, so Docker creates an empty anonymous volume for each
# container of this image (`docker run --rm` removes it, and it is never used).
FROM postgres:16-bookworm@sha256:e8a07d2c7579b038938cdba6c539e580e5001caf9ea203d397b5cc83e76bd938
# The base image has no CA store, and TLS to PostgreSQL (`sslmode=require`, ADR 0018) checks the
# server's certificate against the operating system's roots (a Neon database needs this). The build
# stage has the Debian bundle (the `ca-certificates` package, as of its pinned digest), so it is
# copied: the final stage then needs no network at build time.
# The base also ships Debian's "snakeoil" test certificate in /etc/ssl/certs, whose private key is
# public (the Debian `ssl-cert` package ships it to everyone). It must never be a
# trusted root, so both files and the dangling hash link are removed, and SSL_CERT_FILE makes the
# root loader (rustls-native-certs, and libpq) read the bundle only, not the whole directory.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN rm -f /etc/ssl/certs/ssl-cert-snakeoil.pem /etc/ssl/private/ssl-cert-snakeoil.key \
    && find /etc/ssl/certs -xtype l -delete \
    && useradd --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin forgebase \
    && install -d -o forgebase -g forgebase /app /backups
COPY --from=build /forgebase-app /usr/local/bin/forgebase-app
COPY --from=desk /desk/dist /app/desk
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    FORGEBASE_DESK_DIR=/app/desk \
    FORGEBASE_PG_BIN=/usr/lib/postgresql/16/bin
USER 10001:10001
EXPOSE 8000
# The postgres base image sets SIGINT; both processes of `all` mode also stop on SIGTERM.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["serve"]
