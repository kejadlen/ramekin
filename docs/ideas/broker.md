# Host broker

Status: proposed (2026-11). Nothing is implemented yet. The Rex service was
designed from Rex's public docs, not against a running Rex, so every Rex
detail below needs checking before it's built on.

Some host tools are worth reaching from inside the container but unsafe to
hand over whole. [Rex](https://www.superlogical.com/rex/docs), the terminal
ramekin usually runs in, is the first: an agent that can split a pane, start
a dev server in it, and read it back is more useful, but Rex's socket is
full control of the host account. The broker is one host-side mechanism for
offering a narrow, fixed slice of a tool like that, as MCP tools, with Rex
as its first service.

## Goals

1. **Narrow slices of host tools.** Each service offers a few tools, not
   the whole host tool.
2. **The trusted side decides.** The container sends a request. The host
   validates it, checks it against session state, and makes the real call,
   without a shell.
3. **One mechanism.** Services share the transport, the container-side
   binary, handles, limits, and logging. Adding one is a Rust module, not
   new plumbing.
4. **Off unless the host user turns it on.** A repo can't enable or widen
   a service.
5. **Room for reviewed plans.** Not built now, but nothing here should
   assume every call is decided on the spot. See [Plans](#plans-later).

## Shape

```
container                                    host
---------                                    ----
claude --stdio--> ramekin mcp ─┐
                               ├─MCP─> /run/ramekin/broker.sock ─> ramekin broker
pi --shell--> ramekin call ────┘                                    ├─ rex service ─rex CLI─> Rex server
                                                                    └─ (later services)
```

### The broker

Part of the `ramekin` binary. `run` starts it on a thread before the
container and stops it at teardown. It serves MCP (JSON-RPC 2.0) on a Unix
socket in the session dir, mounted into the container at
`/run/ramekin/broker.sock`. It starts only when at least one service is
enabled and available, so a session with nothing to broker has no socket
and no MCP server.

`tools/list` returns the enabled services' tools. `tools/call` is refused
with an MCP error unless the tool belongs to an enabled service, its
arguments validate against the tool's input schema, and the service's
policy accepts them for this session's state.

Shared limits: a request size cap, a per-call timeout, a concurrency cap,
and a per-service rate limit. Every call and its outcome goes to the
session's tracing log, so the host user can see what an agent asked for.

### The container side: one binary

The image carries the Linux `ramekin` binary, which the release workflow
already builds. Inside the container it's only a client. It has two
subcommands, and both relay to the socket and hold no policy:

- **`ramekin mcp`**: a stdio MCP server that forwards every message to the
  broker socket. Claude finds it through managed MCP config baked into the
  image, next to the managed settings that already set bypass mode, so the
  host's `~/.claude` doesn't change.
- **`ramekin call TOOL [JSON]`**: one `tools/call` from the shell, printing
  the result. `ramekin call --list` prints the tools and their schemas. This
  is how pi gets in, since pi has no built-in MCP client. The ramekin
  prompt lists the tools when the broker is up, and a pi extension can wrap
  `ramekin call` later.

The tools have ramekin's own names (`rex_split`, not `rex split`). They
don't pretend to be the real executables, so nothing in the container
mistakes them for the full tool, and a host skill written for the real
`rex` CLI doesn't apply.

With no broker socket, `ramekin mcp` exits at once with an error and
`ramekin call` fails with "no broker in this ramekin session".

### Services

A service is a Rust module implementing one trait:

```rust
trait Service {
    fn name(&self) -> &'static str;
    /// Whether the host can serve it this session, e.g. Rex needs REX_SESSION.
    fn available(&self, host: &HostEnv) -> bool;
    /// Tool names, descriptions, and JSON input schemas.
    fn tools(&self) -> Vec<Tool>;
    /// Decide on a validated call against this session's state.
    fn check(&self, state: &ServiceState, call: &Call) -> Decision;
    /// Make the call. Only reached after `check` allowed it.
    fn run(&self, state: &mut ServiceState, call: &Call) -> Result<ToolResult>;
    /// Undo what the session created, at teardown.
    fn teardown(&self, state: &mut ServiceState);
}

enum Decision {
    Allow,
    Deny(String),
    // Reserved for plans; nothing returns it yet.
    NeedsApproval,
}
```

`check` and `run` are separate, and `Call` is plain data (tool name plus
JSON arguments), so a call can be decided now and run later. Plans depend on
exactly that.

`ServiceState` gives each service a handle table: things the service
created for this session (Rex panes, say) get short handles like `p1`, and
the agent only ever sees handles. A call naming anything else is denied.
That's the general form of "the agent may touch what it made, and nothing
else."

Services are compiled in rather than defined in config, because their
restrictions are code: ownership tracking, argument checks, and calls that
route back into the container.

### Config

A `broker` block enables services by name. It is honored only in the user
layer (`~/.config/ramekin/`) and in profiles defined there. In the project
layer it's an error, because the repo author is an attacker in the threat
model and must not be able to open host channels.

```kdl
// ~/.config/ramekin/config.kdl
broker {
    rex
}
```

Services listed but not available (no `REX_SESSION`, say) are skipped with
a debug log. `ramekin config` shows which services are enabled and
available.

## Rex service

### Why not mount the Rex socket

Rex's server socket is full control of the host account. Its API can start
terminals running the account's shell (`session.new_block`, `rex run`),
type into any pane (`block.event`, `client.key`), run Lua (`client.action`,
`client.set_script`, `rex do -e`), reload config, redirect hosts, and expose
the server on the tailnet. Rex has no read-only mode, scoped tokens, or
per-method ACLs. Several methods also fall back to the *focused* block when
an id is missing, so even a well-meaning call can land in the user's own
shell. Filtering the raw socket isn't viable either, since its wire format
is undocumented.

So the container never gets the Rex socket, the real `rex` binary, or a Rex
id.

### Tools

Available when `REX_SESSION` is set on the host. The service runs the
host's `rex` CLI, always with `-s "$REX_SESSION"` and an explicit block id,
so the focused-block fallback never applies.

| Tool | Rex call | Restriction |
|---|---|---|
| `rex_split`, `rex_window` (`cmd`, `direction`) | `rex split` / `rex window new` | The pane runs `docker exec -it <container> CMD` (or `container exec` under Apple), never a host shell |
| `rex_capture` (`pane`) | `rex capture` | Owned panes, plus read-only capture of the agent's own pane |
| `rex_send` (`pane`, `text` or `key`) | `rex send` / `rex send-key` | Owned panes only, never the agent's own pane |
| `rex_wait` (`pane`, `pattern` or `exit`, `timeout`) | `rex wait` | Owned panes only, with a timeout cap |
| `rex_close` (`pane`) | `block.close` | Owned panes only |
| `rex_label` (`pane`, `text`) | `set_block_label` | Owned panes and the agent's own pane |
| `rex_list` | `session.view` | Owned panes only |

The first row is what keeps this safe. Every pane the agent opens is
another shell *in the same container*, with the same mounts and network, so
"run a command in a pane" never means "run a command on the host." Typing
into the agent's own pane is refused because it would forge the user's
input to the agent. Teardown closes the panes the session opened, and the
service caps how many it can open.

Lua, config reload, server and host management, tailnet, `attach_all`,
event streams, block data WebSockets, other sessions, other clients, and the
user's own panes aren't tools, so there's nothing to filter.

`REX_SESSION` and `REX_BLOCK` aren't forwarded into the container. Rex's
own skill teaches the real CLI, so it should be hidden in the container
(a `/dev/null` mask over `~/.claude/skills/rex`); the tool descriptions
take its place.

### Program status (OSC 7501)

Rex's status protocol is plain escape sequences on the agent's own TTY, so
it already crosses the container boundary and needs no broker. A Claude
hook or pi extension that emits `working` / `blocked` / `done` would light
up Rex's status UI with nothing from ramekin, so it belongs in host agent
config.

## Plans (later)

Later, or never: the agent sends a *plan* the host user reviews, and only
an approved plan runs. That covers calls too risky to allow on the spot but
fine once a person has read them: a host command, a push, a call into the
user's own Rex panes.

How it would fit:

- **Submitting.** A broker tool, `plan_submit`, takes a title, a Markdown
  rationale, and an ordered list of `Call`s. The broker runs `check` on
  each call. A plan containing a denied call is rejected outright, so
  approval can widen *when* a call runs but never *what* a service allows.
  Calls that would be allowed now could run directly, but stay in the plan
  so it runs in order.
- **Reviewing.** Plans land in a host-side dir, outside every mount, like
  the outbox's sidecars. `ramekin plans list|show|approve|reject` reviews
  them, the same shape as `ramekin outbox`. `show` prints each call exactly
  as it will run, not the agent's summary of it.
- **Running.** `approve` runs the calls in order through `run`, from the
  host user's command, never from the agent. It stops at the first failure
  and records results. The agent sees the outcome through `plan_status`, or
  in the next session.
- **Integrity.** The plan is hashed at submit time, and approve runs the
  hashed calls only, so the agent can't swap a plan after it's been read.
  A plan approved in one session against handles that no longer exist
  fails its checks and doesn't run.

What the first version must get right so this stays possible: `Call` is
serializable data, `check` is separate from `run` and has no side effects,
and `Decision` has room for `NeedsApproval`.

## Threat model delta

A new edge back to the host: container → broker socket → each enabled
service. Every service gets its own entry in `../threat-model.md` when it
lands. For the broker itself, a hijacked agent (A6) can:

- Call any tool of any enabled service. **By design.** That's the slice
  the host user chose, so each service's tools must be safe to hand to an
  attacker.
- Confuse a service into a call outside the slice. **Mitigated** by schema
  validation on the host, a separate `check`, argv built without a shell,
  and unknown arguments refused rather than passed on.
- Have a repo enable a service. **Prevented by design**: `broker` is a
  user-layer-only node.
- Exhaust the broker. **Mitigated** by the size, timeout, concurrency, and
  rate caps.

For Rex specifically, it can clutter the user's Rex session (**Open**, low
impact, capped), and draw panes that look like the user's own to phish them
(**Open**, the same as a malicious TUI in the agent's own pane today;
labeling every agent pane `ramekin: …` reduces it).

Plans, if built, add one more: a plan written to mislead the reviewer.
That's why `show` prints the literal calls and approval can't exceed what
`check` allows.

## Open questions

1. **Socket transport.** A host Unix socket bind-mounted into the container
   works on Linux Docker. Docker Desktop on macOS and Apple's `container`
   both need checking. If either can't pass a host socket in, the fallback
   is the same messages over a spool directory in the session dir, which
   every runtime supports.
2. **Linux binary in the image.** The image would download the matching
   `ramekin` release, or a dev build would copy in a locally built one. The
   container's `ramekin` must stay a pure client, so its run/config code
   paths should refuse to run when `RAMEKIN=1`.
3. **`exec` from a Rex pane.** The new pane runs `docker exec` on the
   host, so it needs the container's name (ramekin knows it) and a Docker
   CLI on the PATH as Rex sees it.
4. **Rex CLI flags.** `rex split --shell=none -- CMD`, `rex capture`,
   `rex wait`, and their JSON output need checking against a real Rex,
   including how `split` reports the new block's id.
