# Nix redesign

Status: proposed (2026-11) — nothing implemented yet. Assumes every repo
ramekin runs against carries a `flake.nix`, and that jj repos are colocated
(so nix sees a `.git`).

## Goals

1. **The repo declares its own environment** — the container's project
   toolchain is exactly the repo's `devShells.default`, with no second copy
   in a `.ramekin/Dockerfile`.
2. **Harness tools stay fresh without hand-maintained pins** — pi and
   claude-code move fast; today every `--rebuild` pulls latest, and the
   redesign shouldn't make that worse.
3. **One mechanism** — tools arrive one way, cache one way, and update one
   way, instead of apt + curl-the-latest-tag + npm + per-repo Dockerfiles.
4. **Keep the sandbox** — profiles, mounts, persistence policies, caches,
   the outbox, and the runtime seam are unaffected.

## Principles

Ramekin today does two jobs: **provisioning** (the base Dockerfile resolving
latest releases with curl and jq, aarch64 hardcoded, git built from source;
then a per-repo `FROM ramekin-agent` layer) and **sandboxing** (everything
else). Nix is good at the first and has nothing to say about the second, so
this redesign touches only provisioning.

It follows the three tiers from `../config-redesign.md`:

- **The binary** keeps the session model and the bootstrap image.
- **The host's existing files** hold content — now including the harness
  tool set, as a flake in dotfiles, next to the agent config it already
  mounts.
- **KDL** keeps the remainder, unchanged.

## Design

### Three layers

1. **Image: a bootstrap.** A static `nix` binary installed outside `/nix`,
   CA certs, the Claude managed settings, `IS_SANDBOX=1`, `RAMEKIN=1`.
   No git, no agents, no tools. It changes almost never, so per-project
   image builds and the GitHub-token BuildKit secret go away.
2. **Harness flake: the agent's tools, on the host.** A flake at
   `~/.config/ramekin/flake.nix` (symlinked from dotfiles), mounted
   read-only into the container like the rest of the host config. It
   layers the harness tools over the project's devShell.
3. **Project flake: the project's toolchain.** The repo's own `flake.nix`,
   riding the workspace mount.

### The harness flake

The harness takes the project as an input it never resolves for real —
ramekin overrides it per session — and builds one shell:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    agents.url = "...";  # a daily-bumped packaging of pi + claude-code
    # Placeholder; ramekin overrides it with the workspace every session.
    project.url = "github:kejadlen/ramekin?dir=nix/empty-project";
  };

  outputs = { nixpkgs, agents, project, ... }:
    let
      forLinux = nixpkgs.lib.genAttrs [ "aarch64-linux" "x86_64-linux" ];
    in {
      devShells = forLinux (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            inputsFrom = pkgs.lib.optional
              (project ? devShells.${system}.default)
              project.devShells.${system}.default;
            packages = [
              agents.packages.${system}.pi
              agents.packages.${system}.claude-code
            ] ++ (with pkgs; [
              git jujutsu ripgrep fd just jq difftastic zizmor bc
            ]);
          };
        });
    };
}
```

- **Git lives here, not in the image.** Nix fetches git repos with its
  built-in libgit2 (2.20+; confirm against the chosen static build), so the
  bootstrap doesn't need a `git` binary, and git's version moves with
  everything else in `flake.lock`.
- **PATH order is the harness author's call**, made inside one `mkShell`.
  The sketch puts project tools first, so a repo that pins its own node or
  git wins.
- **Repos without a flake** just aren't overridden: the placeholder is an
  empty flake ramekin publishes at `nix/empty-project/` (`outputs = _: {}`),
  and the `optional` guard yields the harness tools alone. The same guard
  covers a flake that defines no linux devShell — `ramekin config` should
  say so rather than leave it silent.
- **ranger** isn't in nixpkgs; it needs a flake output of its own (it's
  ours) or a `fetchurl` of its release in the harness.
- Ramekin can ship the sketch above as a template:
  `nix flake init -t github:kejadlen/ramekin`.

### Session

Config resolution is unchanged. Materialization adds:

1. A persistent `ramekin-nix` volume mounted writable at `/nix` — the store,
   shared across sessions and repos.
2. The harness dir mounted read-only (target TBD, e.g.
   `/root/.config/ramekin`).
3. The agent command becomes:

   ```sh
   nix develop /root/.config/ramekin \
     --override-input project /workspace/<slug> \
     --no-write-lock-file \
     --profile /nix/var/ramekin/<slug> \
     -c pi …
   ```

   - The override is the **bare path**, not `path:`. Nix sees a git repo
     and copies only tracked files; `path:` would copy everything,
     including `target/` (which a cache shadows inside the workspace) and
     untracked junk.
   - `--no-write-lock-file` because the harness is read-only. The harness
     `flake.lock` is the host's; overriding `project` doesn't need to
     record anything.
   - `--profile` keeps the built environment as a per-repo GC root and
     warm cache, so a relaunch skips evaluation until a lock changes.

Caches (`cache { target }`) still matter — the host is darwin and the
container linux — but now only invalidate when the pinned toolchain
changes.

### Freshness

Bumping is `nix flake update` on the host, in the harness dir. The change is
a `flake.lock` diff in dotfiles: reviewable, and the same on every machine.
The agents input should be a packaging that someone (a bot, ideally not us)
bumps daily, so pi and claude-code stay within a day of latest with no
hash maintenance. Everything else tracks nixpkgs-unstable.

The project's own `flake.lock` is the project's business; ramekin never
updates it.

### What changes elsewhere

- **Removed:** most of `assets/Dockerfile`, `.ramekin/Dockerfile` support,
  `Runtime::build`'s per-project builds and the `Build` struct, GitHub
  token forwarding, the hardcoded aarch64 downloads.
- **`--rebuild`** means "drop the cached dev profile for this repo."
- **`ramekin config`** reports the harness flake, whether the project has a
  linux devShell, and the dev profile's state, in place of Dockerfile
  status.
- **`ramekin gc`** (new) collects garbage in the store volume, keeping the
  per-repo profiles.
- **`ramekin-prompt.md`:** missing *project* tools go in the repo's
  `flake.nix` (a workspace edit — a normal, reviewable diff); one-offs via
  `nix shell nixpkgs#foo`; problems with *harness* tools go to the outbox,
  like any other read-only shared config.
- **Networking:** sessions now reach cache.nixos.org (and the agents
  input's cache, if it has one) — an allowlist entry once the firewall
  sidecar exists.

## Alternatives considered

- **Ramekin's own flake ships the harness tools** (a pinned `agent-tools`
  set plus a `mkSessionShell` helper). Freshness then depends on bumping
  ramekin's lock, and the tool set isn't the user's to edit without a
  release. The harness-on-host version moves the same flake into dotfiles.
- **Agents outside nix**, npm-installed into a versioned prefix with an
  atomic `current` symlink, everything else via nix. Keeps today's "latest
  on rebuild" exactly, but leaves two mechanisms.
- **Staples from an unlocked nixpkgs** (`nix profile install
  github:NixOS/nixpkgs/nixpkgs-unstable#…` on rebuild). Fresh with no
  hashes, but unreproducible across machines.
- **DotSlash files on the host** for every tool, with git + dotslash in the
  image. Elegant and lazy, but each file pins a URL and hash, so most tools
  need a `ramekin tools update` generator, and pi (an npm package) doesn't
  fit at all.
- **Build a per-project image on the host** with
  `dockerTools.streamLayeredImage`. Needs a linux builder on the Mac and a
  reload on every lock change; in-container builds against the store
  volume need neither and behave the same under both runtimes.

## Sequencing

1. Write the harness flake (and `nix/empty-project/`), and check it covers
   everything `assets/Dockerfile` installs. No ramekin changes.
2. Bootstrap image + store volume + the `nix develop` command, behind a KDL
   switch (`toolchain "nix"`), alongside the Dockerfile path.
3. Dev profile caching, `--rebuild` semantics, `ramekin config` reporting,
   `ramekin gc`, prompt update.
4. Flip the default; warn when a `.ramekin/Dockerfile` is present.
5. Delete the Dockerfile path. Ramekin's own `.ramekin/Dockerfile` (rustup)
   is replaced by a `devShells.default` in this repo's flake.

## Open questions

- **Shared writable store.** Agents run as root in bypass-permissions
  mode, so one session can tamper with store paths another uses. The
  hardening is a nix-daemon sidecar that owns the volume, with agent
  containers mounting the store read-only plus the daemon socket. That
  fits compose (and the planned firewall sidecar) but not Apple's single
  `container run`. Is the unhardened version acceptable to start?
- **Apple containers and shared volumes.** Does nix's sqlite locking hold
  when concurrent Apple-runtime sessions share the store volume? Unverified.
- **Bootstrapping an empty volume.** Docker seeds a named volume from the
  image on first mount; Apple may not. Installing nix outside `/nix` is
  meant to make an empty store fine either way — verify.
- **Packaging pi.** Is there a maintained, daily-bumped packaging (numtide's
  `llm-agents.nix` may cover both agents — unchecked), or do we
  `buildNpmPackage` it ourselves?
- **New files in colocated jj repos.** Nix only sees git-tracked files; a
  file that's new in jj's working-copy commit may not be in git's index, so
  a `flake.nix` importing a just-added `.nix` file could fail to evaluate.
  Test how current jj handles the index.
- **Eval cost on large repos.** Copying tracked files into the store on
  every re-evaluation; the dev profile hides relaunches but not lock
  changes.
- **Harness mount target and config.** Fixed path, or a KDL node to point
  at a different harness dir per machine?
