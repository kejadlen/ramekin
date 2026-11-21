# Rex support

Status: proposed (2026-11). Nothing is implemented yet. It was designed
from Rex's public docs, not against a running Rex, so every Rex detail below
needs checking before it's built on.

[Rex](https://www.superlogical.com/rex/docs) is the terminal ramekin
usually runs in. Rex gives agents a skill and a `rex` CLI for splitting
panes, running commands, reading screens, and waiting on output. That's
worth having inside ramekin: an agent can start a dev server in a side pane,
watch a test run, or show a diff without fighting for its own TTY.

## Goals

1. **The agent can drive Rex**: open panes, run commands in them, read
   them, type into them, close them.
2. **Rex isn't a way out of the sandbox.** Nothing the agent asks Rex for
   may run on the host or touch panes the agent didn't make.
3. **It's inert outside Rex**: no `REX_SESSION`, no Rex plumbing.

## Why not mount the Rex socket

The Rex server socket is full control of the host account. Its API can
start terminals running the account's shell (`session.new_block`,
`rex run`), type into any pane (`block.event`, `client.key`), run Lua
(`client.action`, `client.set_script`, `rex do -e`), reload config,
redirect hosts, and expose the server on the tailnet. Rex has no read-only
mode, no scoped tokens, and no per-method ACLs: who reaches the socket gets
everything ("Access to the shared server is access to the rex account").
Several methods also fall back to the *focused* block when an id is
missing, so even a well-meaning call can land in the user's own shell.

Filtering the raw socket isn't viable either: its wire format is
undocumented, so a proxy would be parsing a moving target.

So the agent never sees the Rex socket, the `rex` binary, or a Rex id it
didn't get from ramekin.

## Design: a broker with a fixed verb set

```
container                          host
---------                          ----
rex (shim) --verb--> broker.sock --> ramekin broker --rex CLI--> Rex server
```

- **The broker** is part of the `ramekin` binary. It starts with the
  session when `REX_SESSION` is set on the host, listens on a socket in the
  session dir, and stops at teardown. It runs the host's `rex` CLI itself,
  always with `-s "$REX_SESSION"` and an explicit block id, so the focused-
  block fallback never applies.
- **The shim** is a small `rex` executable in the image that speaks the
  broker's protocol: one JSON request per connection, one JSON reply. It
  takes the same argument shapes as the real CLI for the verbs below, so
  Rex's own skill mostly works unchanged, and anything else fails with a
  clear "not allowed in ramekin" error.

### Verbs

| Verb | Rex call | Restriction |
|---|---|---|
| `split [-- CMD]`, `new [-- CMD]` | `rex split` / `rex window new` | The pane runs `docker exec -it <container> CMD` (or `container exec` under Apple), never a host shell |
| `capture BLOCK` | `rex capture` | Agent-owned blocks only |
| `send BLOCK TEXT`, `send-key BLOCK KEY` | `rex send` / `rex send-key` | Agent-owned blocks only |
| `wait BLOCK …` | `rex wait` | Agent-owned blocks only, with a timeout cap |
| `close BLOCK` | `block.close` | Agent-owned blocks only |
| `label BLOCK TEXT` | `set_block_label` | Agent-owned blocks and the agent's own block |
| `list` | `session.view` | Filtered to agent-owned blocks |

"Agent-owned" means a block the broker created this session, plus the
agent's own block (`REX_BLOCK`) for `label` and read-only `capture`. The
broker keeps that set in memory and hands the shim short handles (`p1`,
`p2`), not Rex ids. Sending text to the agent's own block is refused, since
typing into it would forge the user's own input to the agent.

The important line is the first row: every pane the agent opens is another
shell *in the same container*, with the same mounts and the same network.
That's what keeps "run a command in a pane" from meaning "run a command on
the host." Pane scrollback stays on the host, but only the agent's own
output is in it.

### What stays out

Lua (`do`, `client.action`, `client.set_script`), config reload, server
and host management, tailnet, `attach_all`, event streams, block data
WebSockets, other sessions, other clients, and the user's own panes. None
of these are in the broker, so there's nothing to filter.

### Program status (OSC 7501)

Status reports are plain escape sequences on the agent's own TTY, so they
already cross the container boundary and need no broker. Rex scopes them to
the agent's terminal. A Claude hook or pi extension that emits
`working` / `blocked` / `done` would light up Rex's status UI with nothing
from ramekin, so it belongs in host agent config, not here.

### Config

On by default when `REX_SESSION` is set, with a KDL switch to turn it off:

```kdl
rex #false
```

The Rex skill rides the existing mounts: `~/.claude/skills/rex` already
mounts read-only for Claude. Pi's copy lives in `~/.agents/skills/rex`,
which ramekin would add to pi's allowlist. `REX_SESSION` and `REX_BLOCK`
aren't forwarded; the shim doesn't need them, and a real `rex` reaching the
container shouldn't find them either.

## Threat model delta

A new edge back to the host: container → broker socket → Rex. A hijacked
agent (A6) can:

- Open panes, which are new shells in the same container. **No new
  capability.** It already has a shell.
- Clutter the user's Rex session with windows and panes. **Open**, low
  impact; cap the count per session.
- Draw things on the user's screen that look like the user's own panes, to
  phish the user into typing a secret or approving something. **Open**, the
  same as a malicious TUI in the agent's own pane today. Labeling every
  agent pane (`ramekin: …`) reduces it.
- Exhaust the broker. **Mitigated** by one request per connection and a
  size cap.

The broker must reject any request naming a block outside the owned set,
and must build its `rex` argv without a shell.

## Open questions

1. **Socket transport.** A host Unix socket bind-mounted into the
   container works on Linux Docker. Docker Desktop on macOS and Apple's
   `container` both need checking. If either can't pass a host socket in,
   the fallback is a request/response spool in a mounted session dir, which
   every runtime supports.
2. **`exec` from a Rex pane.** The new pane runs `docker exec` on the host,
   so it needs the container's name (ramekin knows it) and a Docker CLI on
   the host's PATH as Rex sees it.
3. **Rex CLI flags.** `rex split --shell=none -- CMD`, `rex capture`,
   `rex wait`, and the JSON output shapes need checking against a real
   Rex, including how `split` reports the new block's id.
4. **Pane lifetime.** Agent panes should probably close at teardown. With
   the container gone, their `exec` dies anyway, but a closed pane is
   tidier than a dead one.
