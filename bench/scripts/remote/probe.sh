#!/usr/bin/env bash
# Prints key=value facts about this machine (read-only). Used by preflight.sh:
#   ssh HOST 'bash -s -- <repo dir>' < bench/scripts/remote/probe.sh
repo="${1:-$HOME/rustle}"
iface=$(ip route show default 2>/dev/null | awk '{print $5; exit}')
echo "cpu=$(lscpu | awk -F: '/Model name/{gsub(/^ +/,"",$2); print $2}')"
echo "nproc=$(nproc)"
echo "threads_per_core=$(lscpu | awk -F: '/Thread\(s\) per core/{gsub(/ /,"",$2); print $2}')"
echo "kernel=$(uname -r)"
echo "governor=$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo n/a)"
echo "irqbalance=$(systemctl is-active irqbalance 2>/dev/null || echo absent)"
echo "iface=${iface:-none}"
echo "link_mbit=$(cat "/sys/class/net/$iface/speed" 2>/dev/null || echo n/a)"
echo "mtu=$(cat "/sys/class/net/$iface/mtu" 2>/dev/null || echo n/a)"
echo "offload=$(ethtool -k "$iface" 2>/dev/null | awk '/^(tcp-segmentation-offload|generic-segmentation-offload|generic-receive-offload):/{printf "%s%s ", $1, $2}')"
for k in net.core.somaxconn net.ipv4.ip_local_port_range net.ipv4.tcp_tw_reuse net.core.rmem_max \
         net.core.wmem_max net.ipv4.tcp_rmem net.ipv4.tcp_wmem net.ipv4.tcp_congestion_control \
         vm.max_map_count kernel.pid_max kernel.threads-max; do
    echo "sysctl.$k=$(sysctl -n "$k" 2>/dev/null | tr '\t' ' ')"
done
echo "docker=$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo missing)"
echo "git=$(cd "$repo" 2>/dev/null && git rev-parse --short HEAD 2>/dev/null || echo n/a)"
echo "git_dirty=$(cd "$repo" 2>/dev/null && git status --porcelain 2>/dev/null | wc -l | tr -d ' ' || true)"
[ -d "$repo/.git" ] || echo "git_dirty=n/a"
