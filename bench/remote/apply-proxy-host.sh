#!/usr/bin/env bash
# Proxy host only, as root: keeps the proxy's cores to the proxy.
#   - takes the hyperthread siblings of the proxy cores offline (an idle sibling
#     is still a competitor for the core's execution units);
#   - moves the NIC interrupts to the remaining cores (otherwise they land on
#     the proxy's core, where their time is not charged to the proxy's cgroup).
#
#   ssh HOST 'sudo bash -s -- 0-3 [--dry]' < bench/remote/apply-proxy-host.sh
set -euo pipefail
list="${1:?proxy cpus, e.g. 0 or 0-3}"; dry="${2:-}"
run() { if [[ "$dry" == --dry ]]; then echo "would: $*"; else eval "$@"; fi; }

expand() { local part a b; for part in ${1//,/ }; do a=${part%-*}; b=${part#*-}; seq "$a" "$b"; done; }
mine=" $(expand "$list" | tr '\n' ' ')"
online=" $(expand "$(cat /sys/devices/system/cpu/online)" | tr '\n' ' ')"

sibs=""
for c in $mine; do
    for s in $(expand "$(cat /sys/devices/system/cpu/cpu"$c"/topology/thread_siblings_list)"); do
        [[ "$mine" == *" $s "* || "$sibs" == *" $s "* || "$s" == 0 ]] || sibs="$sibs $s "
    done
done
for s in $sibs; do
    run "echo 0 > /sys/devices/system/cpu/cpu$s/online"; echo "sibling cpu$s of the proxy cores: offline"
done

others=""
for c in $online; do
    [[ "$mine" == *" $c "* || "$sibs" == *" $c "* ]] || others="$others,$c"
done
others="${others#,}"
[[ -n "$others" ]] || { echo "no CPU left for interrupts" >&2; exit 1; }
iface=$(ip route show default | awk '{print $5; exit}')
for irq in $(awk -v i="$iface" '$0 ~ " "i"$|"i"-|"i"[0-9]" {gsub(":","",$1); print $1}' /proc/interrupts); do
    run "echo '$others' > /proc/irq/$irq/smp_affinity_list" && echo "irq $irq -> cpus $others"
done
echo "proxy cores: $list; everything else (interrupts, other processes) on: $others"
