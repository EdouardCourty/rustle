#!/usr/bin/env bash
# Attaches ONE Elastic Metal server to its Private Network (Ubuntu, netplan, run as root or through sudo).
# Public and private traffic share the NIC: the private side is a VLAN interface on it.
#
#   ssh HOST 'sudo bash -s -- <vlan-id> dhcp'            < bench/scripts/remote/private-net.sh
#   ssh HOST 'sudo bash -s -- <vlan-id> 172.16.12.10/22' < bench/scripts/remote/private-net.sh
#
# <vlan-id>   shown in the console, server > "Private Networks" tab (the same on every server of the network).
# dhcp | IP   dhcp = the address the Private Network's IPAM assigned; otherwise a static address with prefix.
# The parent NIC is the one netplan already configures. Prints the interface and its address on the last line. Idempotent.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
vlan="${1:?VLAN id}"; addr="${2:-dhcp}"
name="priv$vlan"   # Linux interface names are limited to 15 characters
# netplan refers to the NIC by the id of its `ethernets:` entry (cloud-init names it eth0, matched by MAC).
rm -f /etc/netplan/51-private-network.yaml
link=$(netplan get ethernets | awk '/^[A-Za-z0-9_.-]+:/ {sub(":", "", $1); print $1; exit}')
[[ -n "$link" ]] || { echo "no ethernets entry in netplan" >&2; exit 1; }

if [[ "$addr" == dhcp ]]; then
    body="      dhcp4: true
      dhcp4-overrides:
        use-routes: false
        use-dns: false"
else
    body="      addresses: [$addr]"
fi
cat > /etc/netplan/51-private-network.yaml <<YAML
network:
  version: 2
  vlans:
    $name:
      id: $vlan
      link: $link
$body
YAML
chmod 600 /etc/netplan/51-private-network.yaml
netplan apply
for _ in $(seq 1 15); do
    ip -4 -br addr show dev "$name" 2>/dev/null | grep -q 'UP.*[0-9]\.' && break
    sleep 1
done
ip -4 -br addr show dev "$name"
