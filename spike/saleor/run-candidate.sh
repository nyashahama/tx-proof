#!/usr/bin/env sh
set -eu

txproof_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
saleor_root="${TIV_SALEOR_SOURCE:-${txproof_root}/../saleor-candidate-source}"
expected_saleor_sha="55c5b8bf8360ffa014d5ab7ee3f1b63f4635b06b"

actual_saleor_sha="$(git -C "$saleor_root" rev-parse HEAD)"
if [ "$actual_saleor_sha" != "$expected_saleor_sha" ]; then
  echo "Saleor candidate SHA mismatch: expected ${expected_saleor_sha}, got ${actual_saleor_sha}" >&2
  exit 1
fi

export TIV_TXPROOF_ROOT="$txproof_root"

docker compose \
  -p txproof-saleor-phase0 \
  -f "${saleor_root}/.worktree-container/docker-compose.yml" \
  -f "${txproof_root}/spike/saleor.compose.yaml" \
  up -d --build db cache stripe-fixture

docker compose \
  -p txproof-saleor-phase0 \
  -f "${saleor_root}/.worktree-container/docker-compose.yml" \
  -f "${txproof_root}/spike/saleor.compose.yaml" \
  exec -T db psql -U saleor -d template1 -c "CREATE EXTENSION IF NOT EXISTS pg_trgm;"

docker compose \
  -p txproof-saleor-phase0 \
  -f "${saleor_root}/.worktree-container/docker-compose.yml" \
  -f "${txproof_root}/spike/saleor.compose.yaml" \
  exec -T db psql -U saleor -d template1 -c "CREATE EXTENSION IF NOT EXISTS btree_gin;"

docker compose \
  -p txproof-saleor-phase0 \
  -f "${saleor_root}/.worktree-container/docker-compose.yml" \
  -f "${txproof_root}/spike/saleor.compose.yaml" \
  exec -T db psql -U saleor -d postgres -c "DROP DATABASE IF EXISTS test_saleor WITH (FORCE);"

docker compose \
  -p txproof-saleor-phase0 \
  -f "${saleor_root}/.worktree-container/docker-compose.yml" \
  -f "${txproof_root}/spike/saleor.compose.yaml" \
  exec -T db psql -U saleor -d postgres -c "DROP DATABASE IF EXISTS test_saleor_replica WITH (FORCE);"

docker compose \
  -p txproof-saleor-phase0 \
  -f "${saleor_root}/.worktree-container/docker-compose.yml" \
  -f "${txproof_root}/spike/saleor.compose.yaml" \
  run --rm saleor \
  python -m pytest /tiv-saleor/test_txproof_commit_close.py -q --reuse-db --rootdir=/app \
    --ds=saleor.tests.settings \
    --disable-socket \
    --allow-hosts=127.0.0.1,::1,cache,db,stripe-fixture,host.docker.internal,host.containers.internal \
    --allow-unix-socket \
    --no-migrations
