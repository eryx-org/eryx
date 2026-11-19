#!/usr/bin/env bash
# One-time host setup for the eryx-desktop CI runner: incus KVM VMs, one
# throwaway VM per job. Run as root on the desktop, after reading it:
#
#   sudo scripts/desktop-runner/setup.sh            # everything, in order
#   sudo scripts/desktop-runner/setup.sh base-image # rebuild the VM image only
#
# What it touches (and nothing else):
#   - pacman: incus, qemu-base, edk2-ovmf
#   - incus: bridge incusci0 (10.251.0.0/24, its own, not Docker's), btrfs
#     pool eryx-ci (a sparse loop file), profile eryx-ci, image eryx-ci-base,
#     block volume cache
#   - firewalld: zone eryx-ci and policy eryx-ci-egress (files in this dir)
#   - iptables DOCKER-USER: two ACCEPT rules for incusci0 <-> the uplink only,
#     via eryx-ci-docker-user.service. Docker's own networks are untouched.
#   - a system user eryx-ci (in incus-admin) that runs the launcher, and
#     /etc/eryx-ci/ for its config and GitHub token
#   - systemd: incus.service drop-in (low CPU/IO weight), the units below
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
POOL_SIZE=${POOL_SIZE:-200GiB}      # sparse; grows as used
CACHE_SIZE=${CACHE_SIZE:-120GiB}    # warm target dirs (Test alone ~45 GB)
VM_DISK=${VM_DISK:-60GiB}           # per VM root, CoW from the image
VM_CPUS=${VM_CPUS:-16}
VM_MEMORY=${VM_MEMORY:-32GiB}
SLOTS=${SLOTS:-2}
UBUNTU=${UBUNTU:-26.04}             # what ubuntu-latest runs
SUBNET=10.251.0               # outside dockerlo's 10.44/16 and Docker's 172.16/12
UPLINK=${UPLINK:-$(ip -4 route show default | awk '{print $5; exit}')}

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
[ -n "$UPLINK" ] || { echo "no default route; set UPLINK" >&2; exit 1; }

packages() {
  pacman -S --needed --noconfirm incus qemu-base edk2-ovmf
  mkdir -p /etc/systemd/system/incus.service.d
  # The VMs' qemu processes are incusd's children: Nice is inherited, and
  # the weights apply if they stay in incus.service's cgroup (check with
  # `systemd-cgls -u incus.service` once a VM runs).
  cat > /etc/systemd/system/incus.service.d/eryx-ci.conf <<'EOF'
[Service]
CPUWeight=20
IOWeight=20
Nice=10
EOF
  systemctl daemon-reload
  systemctl enable --now incus.socket incus.service
}

preflight() {
  if ip -4 addr | grep -q "inet $SUBNET\."; then
    echo "$SUBNET.0/24 is already in use on this host; pick another SUBNET" >&2
    exit 1
  fi
  if command -v dockerlo >/dev/null && dockerlo -allocations 2>/dev/null | grep -q "^$SUBNET\."; then
    echo "dockerlo has allocations in $SUBNET.0/24" >&2
    exit 1
  fi
  local avail_gb
  avail_gb=$(df --output=avail -BG /var/lib | tail -1 | tr -dc 0-9)
  if [ "$avail_gb" -lt "${POOL_SIZE%GiB}" ] && [ "${FORCE:-}" != 1 ]; then
    echo "only ${avail_gb}G free under /var/lib for a $POOL_SIZE pool." >&2
    echo "Free space, lower POOL_SIZE/CACHE_SIZE, or FORCE=1 (a full / breaks the desktop)." >&2
    exit 1
  fi
}

incus_init() {
  incus admin init --preseed <<EOF
networks:
- name: incusci0
  type: bridge
  config:
    ipv4.address: $SUBNET.1/24
    ipv4.dhcp.ranges: $SUBNET.10-$SUBNET.50
    # firewalld does the NAT and filtering (zone + policy below); incus
    # adding its own nftables rules beside firewalld's would be two owners.
    ipv4.nat: "false"
    ipv4.firewall: "false"
    ipv6.address: none
    # Public resolvers, not the host's (which knows tailnet and .test names).
    raw.dnsmasq: |
      no-resolv
      server=1.1.1.1
      server=9.9.9.9
storage_pools:
- name: eryx-ci
  driver: btrfs
  config:
    size: $POOL_SIZE
EOF
  incus profile create eryx-ci 2>/dev/null || true
  incus profile set eryx-ci limits.cpu="$VM_CPUS" limits.memory="$VM_MEMORY"
  incus profile device add eryx-ci root disk pool=eryx-ci path=/ size="$VM_DISK" 2>/dev/null || true
  incus profile device add eryx-ci eth0 nic network=incusci0 name=eth0 2>/dev/null || true
  incus storage volume create eryx-ci cache --type=block size="$CACHE_SIZE" 2>/dev/null || true
}

firewall() {
  install -m 0644 "$HERE/firewalld/eryx-ci.xml" /etc/firewalld/zones/eryx-ci.xml
  sed "s/@UPLINK_ZONE@/$(firewall-cmd --get-zone-of-interface="$UPLINK")/" \
    "$HERE/firewalld/eryx-ci-egress.xml" > /etc/firewalld/policies/eryx-ci-egress.xml
  firewall-cmd --reload
  install -D -m 0755 "$HERE/docker-user.sh" /usr/local/lib/eryx-ci/docker-user.sh
  sed "s/@UPLINK@/$UPLINK/" "$HERE/eryx-ci-docker-user.service" \
    > /etc/systemd/system/eryx-ci-docker-user.service
  systemctl daemon-reload
  systemctl enable --now eryx-ci-docker-user.service
}

launcher() {
  id eryx-ci >/dev/null 2>&1 || useradd --system --home-dir /var/lib/eryx-ci --create-home \
    --shell /usr/bin/nologin --groups incus-admin eryx-ci
  install -d -m 0750 -o eryx-ci -g eryx-ci /etc/eryx-ci
  if [ ! -f /etc/eryx-ci/github-token ]; then
    install -m 0600 -o eryx-ci -g eryx-ci /dev/null /etc/eryx-ci/github-token
    echo "Put the fine-grained token in /etc/eryx-ci/github-token (see the setup doc)."
  fi
  install -D -m 0755 "$HERE/launcher.sh" /usr/local/lib/eryx-ci/launcher.sh
  install -m 0644 "$HERE/eryx-ci-launcher@.service" /etc/systemd/system/
  systemctl daemon-reload
  echo "Start with: for i in \$(seq 1 $SLOTS); do systemctl enable --now eryx-ci-launcher@\$i; done"
}

base_image() {
  local runner_json version sha
  runner_json=$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest)
  version=$(jq -r .tag_name <<<"$runner_json" | sed 's/^v//')
  # Each runner release lists its tarballs' SHA-256s in the release notes.
  sha=$(jq -r .body <<<"$runner_json" | sed -n 's/.*<!-- BEGIN SHA linux-x64 -->\([0-9a-f]\{64\}\)<!-- END SHA linux-x64 -->.*/\1/p')
  [ -n "$sha" ] || { echo "no linux-x64 checksum in runner $version's notes" >&2; exit 1; }

  incus delete -f eryx-ci-build 2>/dev/null || true
  incus init "images:ubuntu/$UBUNTU/cloud" eryx-ci-build --vm -p eryx-ci
  incus config set eryx-ci-build cloud-init.user-data - <<EOF
#cloud-config
users:
  - name: runner
    uid: "1001"
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
package_update: true
packages: [build-essential, clang, lld, cmake, pkg-config, libssl-dev, git, curl, jq,
           zstd, unzip, xz-utils, file, python3, python3-venv, python3-pip,
           ca-certificates, e2fsprogs]
runcmd:
  - mkdir -p /home/runner/actions-runner /cache
  - curl -fsSL -o /tmp/runner.tgz https://github.com/actions/runner/releases/download/v$version/actions-runner-linux-x64-$version.tar.gz
  - echo "$sha  /tmp/runner.tgz" | sha256sum -c -
  - tar -xzf /tmp/runner.tgz -C /home/runner/actions-runner
  - /home/runner/actions-runner/bin/installdependencies.sh
  - chown -R runner:runner /home/runner
  - rm /tmp/runner.tgz
EOF
  incus start eryx-ci-build
  until incus exec eryx-ci-build -- true 2>/dev/null; do sleep 2; done
  incus exec eryx-ci-build -- cloud-init status --wait
  incus exec eryx-ci-build -- cloud-init clean --logs
  incus stop eryx-ci-build
  incus image delete eryx-ci-base 2>/dev/null || true
  incus publish eryx-ci-build --alias eryx-ci-base
  incus delete eryx-ci-build
  echo "eryx-ci-base: Ubuntu $UBUNTU, actions/runner $version"
}

case "${1:-all}" in
  all) preflight; packages; incus_init; firewall; launcher; base_image ;;
  base-image) base_image ;;
  *) echo "usage: $0 [all|base-image]" >&2; exit 2 ;;
esac
