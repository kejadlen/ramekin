ARG BASE
FROM ${BASE}

RUN --mount=type=cache,target=/root/.npm \
    npm install -g @anthropic-ai/claude-code

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
