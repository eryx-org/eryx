#!/usr/bin/env bash
# One runner slot: forever, start a fresh VM, run one job in it with a
# single-use (JIT) runner registration, then delete the VM.
#
# The GitHub token never enters the VM; only the JIT config does, which is
# good for that one runner and one job. Each VM gets a copy-on-write clone
# of the warm cache volume, and only a clone used by a successful push to
# main (checked through the API, not by anything the VM says) replaces it.
set -euo pipefail

slot=$1
repo=eryx-org/eryx
labels='["self-hosted","linux","x64","eryx-desktop"]'
token=$(cat /etc/eryx-ci/github-token)
lock=/var/lib/eryx-ci/cache.lock
[ -n "$token" ] || { echo "empty /etc/eryx-ci/github-token" >&2; exit 1; }

api() {
  curl -fsSL -H "Authorization: Bearer $token" -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" "$@"
}

# Whether the job this runner ran belongs to a successful push to main.
ran_main_push() {
  local name=$1 run
  for run in $(api "https://api.github.com/repos/$repo/actions/runs?branch=main&event=push&per_page=5" | jq '.workflow_runs[].id'); do
    if api "https://api.github.com/repos/$repo/actions/runs/$run/jobs?per_page=100" \
        | jq -e --arg n "$name" '.jobs[] | select(.runner_name == $n and .conclusion == "success")' >/dev/null; then
      return 0
    fi
  done
  return 1
}

while true; do
  name="eryx-desktop-$slot-$(date +%s)"
  vol="cache-$name"
  cleanup() {
    incus delete -f "$name" >/dev/null 2>&1 || true
    incus storage volume delete eryx-ci "$vol" >/dev/null 2>&1 || true
  }
  trap cleanup EXIT

  jit=$(api -X POST "https://api.github.com/repos/$repo/actions/runners/generate-jitconfig" \
    -d "{\"name\":\"$name\",\"runner_group_id\":1,\"labels\":$labels,\"work_folder\":\"_work\"}" \
    | jq -r .encoded_jit_config)

  flock "$lock" incus storage volume copy eryx-ci/cache "eryx-ci/$vol"
  incus init eryx-ci-base "$name" --vm -p eryx-ci
  incus config device add "$name" cache disk pool=eryx-ci source="$vol" >/dev/null
  incus start "$name"
  until incus exec "$name" -- true 2>/dev/null; do sleep 2; done

  # The cache is a block volume: format it the first time, mount it at /cache.
  # shellcheck disable=SC2016 # expands in the VM
  incus exec "$name" -- sh -euc '
    dev=$(readlink -f /dev/disk/by-id/*incus_cache)
    blkid "$dev" >/dev/null || mkfs.ext4 -q -L eryx-cache "$dev"
    mount "$dev" /cache && chown runner:runner /cache'

  # A job that never ends is killed with its VM after 6 h.
  # (On stdin, so it isn't in the host's process list.)
  # shellcheck disable=SC2016 # expands in the VM
  timeout 6h incus exec "$name" --user 1001 --cwd /home/runner/actions-runner \
    --env HOME=/home/runner -- sh -c 'read -r jit; exec ./run.sh --jitconfig "$jit"' <<<"$jit" || true
  incus delete -f "$name" >/dev/null 2>&1 || true

  sleep 10 # let the job's conclusion reach the API
  if ran_main_push "$name"; then
    # ponytail: two main jobs finishing together each promote their own
    # clone, so the earlier one's writes are lost (its target dir is just
    # staler next run). Per-job volumes if that ever matters.
    flock "$lock" sh -c "incus storage volume delete eryx-ci cache &&
      incus storage volume rename eryx-ci '$vol' cache"
    echo "$name: promoted its cache"
  fi
  cleanup
  trap - EXIT
done
