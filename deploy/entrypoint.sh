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
# No arguments means `serve` (the image sets CMD ["serve"] too).
[ "$#" -gt 0 ] || set -- serve
mode="$1"

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

# Single-container mode. Both processes stop gracefully on SIGTERM; a second signal makes them exit at
# once. This script forwards TERM to both children on EVERY TERM or INT it receives (so two signals to
# the container reach the children as two, which is what "exit now" means for them), and sends one TERM
# itself when a child exits on its own. It is PID 1 and reaps its children with `wait`.
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

# Returns when either child exits ($who is its PID) or when a trapped signal arrives ($who is empty).
set +e
who=
wait -n -p who
first=$?
if [ -z "${who:-}" ]; then
  first=   # interrupted by a signal: the children are being stopped, no child status yet
elif [ "$stopping" = 0 ]; then
  echo "entrypoint: a process exited (status $first), stopping the other" >&2
  stop_children
fi

# A trapped signal also interrupts `wait PID` (status above 128, the child not reaped): wait again
# until the PID is really gone. A child that `wait -n` reaped is gone already and keeps its status.
serve_rc=0
worker_rc=0
while kill -0 "$serve_pid" 2>/dev/null; do wait "$serve_pid"; serve_rc=$?; done
while kill -0 "$worker_pid" 2>/dev/null; do wait "$worker_pid"; worker_rc=$?; done

# The first child that exited on its own decides the status (even if a signal arrived afterwards).
if [ -n "$first" ]; then
  exit "$first"
fi
# Stopped on request before any child exited: success unless a child failed while stopping.
[ "$serve_rc" = 0 ] || exit "$serve_rc"
exit "$worker_rc"
