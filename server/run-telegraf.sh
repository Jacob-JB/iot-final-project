#!/bin/sh
# Runs Telegraf with the credentials in server/.env. Extra arguments are passed through, so
# `./server/run-telegraf.sh --test --test-wait 10` prints what would be written without writing it.
set -e
cd "$(dirname "$0")"

if [ ! -f .env ]; then
  echo "server/.env is missing: copy server/.env.example and fill it in" >&2
  exit 1
fi

set -a
. ./.env
set +a

exec telegraf --config telegraf.conf "$@"
