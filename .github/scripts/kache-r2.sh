#!/usr/bin/env bash
# Move kache cache entries from a CI build job to the R2 bucket without giving
# the build job a write key. DIR is a kache filesystem remote
# (artifacts/v3/{packs,manifests}/...), see .github/actions/kache-export.
#
#   kache-r2.sh seed   DIR  (build job, read key)   placeholder manifests for
#                           every key R2 has, so `kache sync --push` into DIR
#                           only writes new entries
#   kache-r2.sh strip  DIR  (build job)              drop the placeholders
#   kache-r2.sh upload DIR  (upload job, write key)  put entries, never overwrite
#
# Seeding works because sync decides what to push from the manifest listing
# alone: with 3,598 of 3,608 keys seeded it pushed 10, in 1.8s instead of
# 2m38s. strip and upload check every path against kache's own key rules
# (cache_key.rs); in the upload job DIR is another job's untrusted output.
#
# Env: KACHE_R2_BUCKET, KACHE_R2_ENDPOINT, AWS_ACCESS_KEY_ID,
#      AWS_SECRET_ACCESS_KEY (AWS_DEFAULT_REGION defaults to R2's "auto").
set -euo pipefail

mode=${1:?usage: kache-r2.sh seed|strip|upload DIR}
dir=${2:?usage: kache-r2.sh seed|strip|upload DIR}
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-auto}

crate='[A-Za-z0-9_.-]{1,128}'
key='[0-9a-f]{64}'
valid="^artifacts/v3/(packs/$crate/$key\\.tar\\.zst|manifests/$crate/$key\\.json)\$"

s3() {
  aws s3api "$@" --bucket "${KACHE_R2_BUCKET:?}" --endpoint-url "${KACHE_R2_ENDPOINT:?}"
}

# Fail on anything that isn't a regular file at a path kache itself would write.
check() {
  if find . -mindepth 1 ! -type f ! -type d | grep -q .; then
    echo "::error::non-regular files in $dir:" >&2
    find . -mindepth 1 ! -type f ! -type d >&2
    exit 1
  fi
  bad=$(find . -type f -printf '%P\n' | grep -Ev "$valid" || true)
  if [ -n "$bad" ]; then
    echo "::error::unexpected paths in $dir:" >&2
    echo "$bad" >&2
    exit 1
  fi
}

summary() {
  echo "$1: $(find . -path '*/packs/*' -type f | wc -l) packs, $(du -sh . | cut -f1)"
}

case $mode in
seed)
  mkdir -p "$dir" && cd "$dir"
  # list-objects-v2 paginates on its own; keys come back tab-separated, or as
  # "None" for an empty bucket, which grep drops (no match is fine here).
  s3 list-objects-v2 --prefix artifacts/v3/manifests/ --query 'Contents[].Key' --output text |
    tr '\t' '\n' | { grep -E "$valid" || true; } | while read -r k; do
      mkdir -p "$(dirname "$k")" && : >"$k"
    done
  echo "seeded $(find . -type f | wc -l) placeholder manifests from R2"
  ;;
strip)
  [ -d "$dir" ] || exit 0
  cd "$dir"
  # Real manifests are never empty; placeholders always are.
  find . -path './artifacts/v3/manifests/*' -type f -size 0 -delete
  find . -type d -empty -delete
  [ -d artifacts ] || { echo "no new entries"; exit 0; }
  check
  summary "new entries"
  ;;
upload)
  [ -d "$dir/artifacts" ] || { echo "no new entries"; exit 0; }
  cd "$dir"
  check
  summary "to upload"
  # Packs before manifests, so a reader never finds a manifest whose pack is
  # missing. --no-overwrite (AWS CLI >= 2.33) skips keys R2 already has and
  # sends every put with If-None-Match: *, so it is create-only like kache's
  # own put_if_absent. One multithreaded process per kind: a process per
  # object took 18 minutes for the first 3,582.
  aws configure set default.s3.max_concurrent_requests 32
  for kind in packs manifests; do
    [ -d "artifacts/v3/$kind" ] || continue
    # A failed put fails the assignment, and set -e the script.
    out=$(aws s3 cp --recursive --no-overwrite --no-progress \
      --endpoint-url "${KACHE_R2_ENDPOINT:?}" \
      "artifacts/v3/$kind" "s3://${KACHE_R2_BUCKET:?}/artifacts/v3/$kind")
    echo "uploaded $(grep -c '^upload:' <<<"$out" || true) new $kind"
  done
  ;;
*)
  echo "unknown mode: $mode" >&2
  exit 2
  ;;
esac
