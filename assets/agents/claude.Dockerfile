ARG BASE
FROM ${BASE}

# The standalone binary, so Claude doesn't need Node. Checked against the
# release manifest the way the official installer does. The auto-updater
# would only rewrite the container's throwaway filesystem.
ENV DISABLE_AUTOUPDATER=1
RUN base=https://downloads.claude.ai/claude-code-releases \
    && version=$(curl -fsSL "$base/latest") \
    && checksum=$(curl -fsSL "$base/$version/manifest.json" \
      | jq -er '.platforms["linux-arm64"].checksum') \
    && curl -fsSL -o /usr/local/bin/claude "$base/$version/linux-arm64/claude" \
    && echo "$checksum  /usr/local/bin/claude" | sha256sum -c - \
    && chmod +x /usr/local/bin/claude

# Claude Code refuses --dangerously-skip-permissions when running as root
# unless IS_SANDBOX=1 acknowledges the sandbox. The container is the sandbox.
ENV IS_SANDBOX=1

# Run Claude in bypass-permissions mode and skip the bypass dialog. Managed
# settings is the highest tier, so this overrides whatever's in the user's
# mounted ~/.claude. defaultMode replaces the --dangerously-skip-permissions
# CLI flag; skipDangerousModePermissionPrompt suppresses the once-per-machine
# acceptance dialog. The container is already a sandbox (see IS_SANDBOX), so
# prompting is redundant.
RUN mkdir -p /etc/claude-code && cat > /etc/claude-code/managed-settings.json <<'EOF'
{
  "permissions": { "defaultMode": "bypassPermissions" },
  "skipDangerousModePermissionPrompt": true
}
EOF
