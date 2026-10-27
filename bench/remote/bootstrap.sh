#!/usr/bin/env bash
# Prepares ONE benchmark machine (Ubuntu 24.04+ / Debian 12, run as root or through sudo). Idempotent.
# Same on the generator, proxy and origin hosts: the three must run identical settings.
#
#   ssh HOST 'sudo bash -s' < bench/remote/bootstrap.sh
#
# Installs Docker and the measuring tools, writes the sysctls (REMOTE.md) to
# /etc/sysctl.d, switches the CPU governor to performance and stops irqbalance.
# Nothing here depends on the proxy under test.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq docker.io iperf3 ethtool git util-linux procps >/dev/null
systemctl enable --now docker >/dev/null 2>&1 || true
# Run through sudo by a non-root login (Scaleway images: `ubuntu`): let that user drive Docker.
[[ -n "${SUDO_USER:-}" && "$SUDO_USER" != root ]] && usermod -aG docker "$SUDO_USER"

cat > /etc/sysctl.d/99-rustle-bench.conf <<'CONF'
net.core.somaxconn = 65535
net.ipv4.ip_local_port_range = 1024 65535
net.ipv4.tcp_tw_reuse = 1
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
# Thread-per-connection proxies (3proxy, Tinyproxy) hit these walls long before 25,000 tunnels.
vm.max_map_count = 1048576
kernel.pid_max = 4194304
kernel.threads-max = 4194304
CONF
sysctl --system >/dev/null

# Fixed frequency: a governor that ramps up and down is noise.
for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    [[ -w "$g" ]] && echo performance > "$g" || true
done
# Interrupts are placed explicitly by preflight.sh (APPLY=1), not by a daemon.
systemctl disable --now irqbalance >/dev/null 2>&1 || true

echo "bootstrap ok: $(docker --version | cut -d, -f1), $(nproc) CPUs, governor $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo n/a)"
