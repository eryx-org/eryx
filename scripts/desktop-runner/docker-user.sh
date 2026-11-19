#!/usr/bin/env bash
# Docker sets the iptables FORWARD policy to DROP, which also drops the CI
# VMs' forwarded traffic (a separate base chain from firewalld's, and both
# must accept). DOCKER-USER is Docker's hook for exactly this; these two
# rules only let incusci0 <-> the uplink through, and firewalld's eryx-ci
# policy still filters destinations.
set -euo pipefail
action=$1 uplink=$2
rules=("-i incusci0 -o $uplink -j ACCEPT"
       "-i $uplink -o incusci0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT")
for r in "${rules[@]}"; do
  # shellcheck disable=SC2086 # each rule is a list of iptables words
  case $action in
    add) iptables -C DOCKER-USER $r 2>/dev/null || iptables -I DOCKER-USER $r ;;
    del) iptables -D DOCKER-USER $r 2>/dev/null || true ;;
  esac
done
