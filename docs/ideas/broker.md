# Host broker

Status: proposed (2026-11). Nothing is implemented yet. The Rex service was
designed from Rex's public docs, not against a running Rex, so every Rex
detail below needs checking before it's built on.

Some host tools are worth reaching from inside the container but unsafe to
hand over whole. [Rex](https://www.superlogical.com/rex/docs), the terminal
ramekin usually runs in, is the first: an agent that can split a pane, start
a dev server in it, and read it back is more useful, but Rex's socket is
full control of the host account. The broker is one host-side mechanism for
offering a narrow, fixed slice of a tool like that, with Rex as its first
service.

## Goals

1. **Narrow slices of host tools.** Each service offers a few verbs, not
   the tool.
2. **The trusted side decides.** The container sends a request. The host
   parses it, checks it, and builds the real call, without a shell.
3. **One mechanism.** Services share the transport, the shim, handles,
   limits, and logging. Adding one is a Rust module, not new plumbing.
4. **Off unless the host user turns it on.** A repo can't enable or widen
   a service.

## Shape

```
container                                host
---------                                ----
rex (shim) --argv--> /run/ramekin/broker.sock --> ramekin broker
                                                    ├─ rex service --rex CLI--> Rex server
                                                    └─ open service --open--> browser
```

### The broker

Part of the `ramekin` binary. `run` starts it on a thread before the
container and stops it at teardown. It listens on a Unix socket in the
session dir, mounted into the container at `/run/ramekin/broker.sock`. It
starts only when at least one service is enabled and available, so a session
with nothing to broker has no socket.

The protocol is HTTP over the Unix socket, so it's debuggable with
`curl --unix-socket`:

```
POST /rex
{"argv": ["split", "--", "cargo", "watch"]}

200
{"stdout": "p1\n", "stderr": "", "status": 0}
```

The body is the shim's raw argv. Parsing happens in the broker, against the
service's own grammar, so the container side holds no policy at all. A
request is refused (403, with a message the shim prints) unless the service
is enabled, the verb is one the service offers, and every argument passes
its checks.

Shared limits: a request size cap, a per-request timeout, a concurrency
cap, and a per-service rate limit. Every request and its outcome goes to the
session's tracing log, so `ramekin` can show what an agent asked for.

### The shim

One small executable in the image, `/usr/local/bin/ramekin-shim`, with a
symlink per service (`/usr/local/bin/rex -> ramekin-shim`). It posts its
argv to `/<argv[0]>`, prints the reply's stdout and stderr, and exits with
its status. With no broker socket it exits 127 with "rex isn't available in
this ramekin session", the same as a missing command.

Because the shim reuses the real tool's name and argument shapes, a host
skill written for the real tool (Rex ships one) mostly works unchanged, and
anything outside the slice fails with a clear message instead of a hang.

### Services

A service is a Rust module implementing one trait:

```rust
trait Service {
    fn name(&self) -> &'static str;
    /// Whether the host can serve it this session, e.g. Rex needs REX_SESSION.
    fn available(&self, host: &HostEnv) -> bool;
    /// Parse argv, check it against this session's state, run it.
    fn call(&self, session: &mut ServiceState, argv: &[String]) -> Result<Reply>;
    /// Undo what the session created, at teardown.
    fn teardown(&self, session: &mut ServiceState);
}
```

`ServiceState` gives each service a handle table: things the service
created for this session (Rex panes, say) get short handles like `p1`, and
the agent only ever sees handles. A request naming anything else is
refused. That's the general form of "the agent may touch what it made, and
nothing else."

Services are compiled in rather than defined in config, because their
restrictions are code: ownership tracking, argument grammars, and calls
that route back into the container.

### Config

A `broker` block enables services by name. It is honored only in the user
layer (`~/.config/ramekin/`) and profiles defined there. In the project
layer it's an error, because the repo author is an attacker in the threat
model and must not be able to open host channels.

```kdl
// ~/.config/ramekin/config.kdl
broker {
    rex
    open
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

### Verbs

Available when `REX_SESSION` is set on the host. The service runs the
host's `rex` CLI, always with `-s "$REX_SESSION"` and an explicit block id,
so the focused-block fallback never applies.

| Verb | Rex call | Restriction |
|---|---|---|
| `split [-- CMD]`, `new [-- CMD]` | `rex split` / `rex window new` | The pane runs `docker exec -it <container> CMD` (or `container exec` under Apple), never a host shell |
| `capture PANE` | `rex capture` | Owned panes, plus read-only capture of the agent's own pane |
| `send PANE TEXT`, `send-key PANE KEY` | `rex send` / `rex send-key` | Owned panes only, never the agent's own pane |
| `wait PANE …` | `rex wait` | Owned panes only, with a timeout cap |
| `close PANE` | `block.close` | Owned panes only |
| `label PANE TEXT` | `set_block_label` | Owned panes and the agent's own pane |
| `list` | `session.view` | Owned panes only |

The first row is what keeps this safe. Every pane the agent opens is
another shell *in the same container*, with the same mounts and network, so
"run a command in a pane" never means "run a command on the host." Typing
into the agent's own pane is refused because it would forge the user's
input to the agent. Teardown closes the panes the session opened, and caps
how many it can open.

Lua, config reload, server and host management, tailnet, `attach_all`,
event streams, block data WebSockets, other sessions, other clients, and the
user's own panes aren't verbs, so there's nothing to filter.

`REX_SESSION` and `REX_BLOCK` aren't forwarded into the container. The
Rex skill rides existing mounts: `~/.claude/skills/rex` already mounts for
Claude, and pi's copy in `~/.agents/skills/rex` would join pi's allowlist.

### Program status (OSC 7501)

Rex's status protocol is plain escape sequences on the agent's own TTY, so
it already crosses the container boundary and needs no broker. A Claude
hook or pi extension that emits `working` / `blocked` / `done` would light
up Rex's status UI with nothing from ramekin, so it belongs in host agent
config.

## Later services

Examples of what fits the same mold, none planned yet:

- **`open URL`**: open an `https://` URL in the host browser, for OAuth
  flows and previews. Scheme-checked, no `file://`.
- **`notify TEXT`**: a desktop notification, rate-limited.
- **`pbcopy`**: write (never read) the host clipboard, size-capped.

Each one is a slice the agent can't build from inside the container, and
small enough to name every verb.

## Threat model delta

A new edge back to the host: container → broker socket → each enabled
service. Every service gets its own entry in `../threat-model.md` when it
lands. For the broker itself, a hijacked agent (A6) can:

- Call any verb of any enabled service. **By design.** That's the slice the
  host user chose, so each service's verbs must be safe to hand to an
  attacker.
- Confuse a service's parser into a call outside the slice. **Mitigated**
  by parsing on the host against a fixed grammar, building argv without a
  shell, and refusing unknown verbs and flags rather than passing them on.
- Have a repo enable a service. **Prevented by design**: `broker` is a
  user-layer-only node.
- Exhaust the broker. **Mitigated** by the size, timeout, concurrency, and
  rate caps.

For Rex specifically, it can clutter the user's Rex session (**Open**, low
impact, capped), and draw panes that look like the user's own to phish them
(**Open**, the same as a malicious TUI in the agent's own pane today;
labeling every agent pane `ramekin: …` reduces it).

## Open questions

1. **Socket transport.** A host Unix socket bind-mounted into the container
   works on Linux Docker. Docker Desktop on macOS and Apple's `container`
   both need checking. If either can't pass a host socket in, the fallback
   is the same requests over a spool directory in the session dir, which
   every runtime supports.
2. **Shim binary.** The shim could be the Linux `ramekin` binary itself
   (dispatching on `argv[0]`), already built by the release workflow, or a
   few lines of shell around `curl --unix-socket`.
3. **`exec` from a Rex pane.** The new pane runs `docker exec` on the
   host, so it needs the container's name (ramekin knows it) and a Docker
   CLI on the PATH as Rex sees it.
4. **Rex CLI flags.** `rex split --shell=none -- CMD`, `rex capture`,
   `rex wait`, and their JSON output need checking against a real Rex,
   including how `split` reports the new block's id.
