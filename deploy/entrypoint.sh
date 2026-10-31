#!/bin/bash
# Entrypoint of the Forgebase image (see the Dockerfile and docs/guide/deploy.md).
#
#   entrypoint.sh serve | worker | <any other command>   exec the binary with these arguments
#   entrypoint.sh all                                     single-container mode: serve and worker together
#
# Environment (all optional here; the binary reads the rest, for example FORGEBASE_DATABASE_URL):
#   FORGEBASE_MIGRATE_ON_START=1  run `migrate` first when the mode starts `serve` (serve, all). Default: off.
#   FORGEBASE_LISTEN              address of `serve`. If unset: 0.0.0.0:$PORT when PORT is set (Render and
#                                 similar hosts), else 0.0.0.0:8000.
#
# `bash` (not sh) because `all` mode uses `wait -n`.
set -eu

app=/usr/local/bin/forgebase-app
mode="${1:-serve}"

if [ -z "${FORGEBASE_LISTEN:-}" ]; then
  export FORGEBASE_LISTEN="0.0.0.0:${PORT:-8000}"
fi

case "$mode" in
  serve | all)
    if [ "${FORGEBASE_MIGRATE_ON_START:-0}" = "1" ]; then
      echo "entrypoint: FORGEBASE_MIGRATE_ON_START=1, running migrate" >&2
      "$app" migrate   # set -e: a failed migration stops the container with its exit code
    fi
    ;;
esac

if [ "$mode" != "all" ]; then
  exec "$app" "$@"
fi

# Single-container mode. Both processes stop gracefully on SIGTERM; a SECOND signal makes them exit at
# once, so each child gets exactly one TERM. This script is PID 1 and reaps its children with `wait`.
if [ "$#" -ne 1 ]; then
  echo "entrypoint: all takes no arguments (use the environment and a config file)" >&2
  exit 2
fi
"$app" serve &
serve_pid=$!
"$app" worker &
worker_pid=$!

stopping=0
stop_children() {
  kill -TERM "$serve_pid" "$worker_pid" 2>/dev/null || true
}
on_signal() {
  stopping=1
  stop_children
}
trap on_signal TERM INT

# Returns when either child exits, or when a trapped signal arrives.
set +e
wait -n
first=$?
if [ "$stopping" = 0 ]; then
  echo "entrypoint: a process exited (status $first), stopping the other" >&2
  stop_children
fi
# 127: that child was already reaped by `wait -n`.
wait "$serve_pid"
serve_rc=$?
wait "$worker_pid"
worker_rc=$?

if [ "$stopping" = 1 ]; then
  # Stopped on request: success unless a child failed while stopping.
  for rc in "$serve_rc" "$worker_rc"; do
    if [ "$rc" != 0 ] && [ "$rc" != 127 ]; then
      exit "$rc"
    fi
  done
  exit 0
fi
exit "$first"
