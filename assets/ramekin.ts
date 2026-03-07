import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  const firewall = process.env.RAMEKIN_FIREWALL !== "false";

  let context = `
# Ramekin Container Environment

You are running inside a Docker container managed by **ramekin**.

## Workspace

The project workspace is bind-mounted at \`/workspace\`. This is the only directory where your changes are visible to the host.

## Filesystem

The container filesystem is ephemeral. Any files written outside \`/workspace\` will be lost when the session ends. System packages installed with \`apt-get\` do not persist across sessions — use a custom \`.ramekin/Dockerfile\` to add permanent dependencies.
`;

  if (firewall) {
    context += `
## Networking

Networking is restricted by an nftables firewall. Only outbound connections to \`api.anthropic.com:443\` are allowed. You cannot fetch URLs, install packages from remote registries, or reach any other external host. All other outbound traffic is blocked.
`;
  }

  pi.on("before_agent_start", async (event) => {
    return { systemPrompt: event.systemPrompt + "\n" + context };
  });
}
