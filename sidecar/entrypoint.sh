#!/bin/sh
set -eu

# Sidecar entrypoint: configure iptables to restrict outbound traffic
# to Anthropic API only, then start the bridge server.

echo "Resolving api.anthropic.com..."
ANTHROPIC_IPS=$(getent hosts api.anthropic.com | awk '{print $1}' | sort -u)

if [ -z "$ANTHROPIC_IPS" ]; then
    echo "ERROR: could not resolve api.anthropic.com"
    exit 1
fi

echo "Anthropic API IPs: $ANTHROPIC_IPS"

# Flush existing rules
iptables -F OUTPUT
iptables -F INPUT

# Default policy: drop all outbound
iptables -P OUTPUT DROP
iptables -P INPUT DROP

# Allow loopback (needed for bridge server health checks, etc.)
iptables -A OUTPUT -o lo -j ACCEPT
iptables -A INPUT -i lo -j ACCEPT

# Allow DNS resolution (needed to keep resolving api.anthropic.com)
iptables -A OUTPUT -p udp --dport 53 -j ACCEPT
iptables -A OUTPUT -p tcp --dport 53 -j ACCEPT
iptables -A INPUT -p udp --sport 53 -j ACCEPT
iptables -A INPUT -p tcp --sport 53 -j ACCEPT

# Allow outbound to Anthropic API IPs on HTTPS (443)
for ip in $ANTHROPIC_IPS; do
    echo "Allowing HTTPS to $ip"
    iptables -A OUTPUT -p tcp -d "$ip" --dport 443 -j ACCEPT
done

# Allow established/related connections back in
iptables -A INPUT -m state --state ESTABLISHED,RELATED -j ACCEPT

# Allow inbound connections from the agent container to the bridge server
# The agent connects to us on the bridge port (default 8080)
BRIDGE_PORT="${BRIDGE_PORT:-8080}"
iptables -A INPUT -p tcp --dport "$BRIDGE_PORT" -j ACCEPT
iptables -A OUTPUT -m state --state ESTABLISHED,RELATED -j ACCEPT

echo "iptables rules configured. Outbound restricted to Anthropic API."
echo "Bridge server port: $BRIDGE_PORT"

iptables -L -n -v

# Start the bridge server
exec /usr/local/bin/bridge
