# Profiles carry agent args

Status: design

## Problem

Pi selects Amazon Bedrock through CLI flags: `pi --provider amazon-bedrock
--model <id>`. No environment variable sets the provider or model; the only
alternatives to the flags are `defaultProvider`/`defaultModel` in pi's
`settings.json`. AWS credentials, by contrast, are entirely environment- and
file-based (`AWS_PROFILE`, `AWS_REGION`, the `~/.aws` directory).

A ramekin profile currently carries three things: `agent`, `env`, and
`mounts`. That covers the credential half of a Bedrock setup but not the
provider-selection half. Today the flags reach pi only through the
per-invocation escape hatch (`ramekin -- --provider amazon-bedrock`), so a
"Bedrock profile" can't actually pick Bedrock on its own — the user retypes
the flags every run.

## Goal

Let a profile pin the agent CLI flags it needs, so selecting the profile is
enough to run against the intended backend. The immediate use is a
`pi-bedrock` profile, but the field is agent-agnostic.

## Design

### The `args` field

A profile gains an optional `args` field: a list of strings passed verbatim
to the agent binary.

```kdl
profile "pi-bedrock" {
    agent "pi"
    args "--provider" "amazon-bedrock"
    env {
        AWS_PROFILE "REPLACE_ME"
        AWS_REGION "REPLACE_ME"
    }
    mounts {
        "~/.aws"
    }
}
```

An `args` node's entries are its arguments, mirroring how `env` and `mounts`
blocks accumulate: multiple `args` nodes within one profile concatenate in
order. Each entry must be a bare string; properties or non-string values are
an error.

### Merge semantics

Args belong to the profile definition, so they follow the existing
profile-merge rule unchanged: profiles merge by name, and a later layer that
redefines a profile replaces the whole definition, args included. Args do
not merge across layers the way `env` and `mounts` do — there is one active
profile, and its args are the profile's args.

### Command assembly

The container command changes from

```
[prompt_flag, PROMPT_TARGET] ++ cli_args
```

to

```
[prompt_flag, PROMPT_TARGET] ++ profile.args ++ cli_args
```

The trailing CLI args (`ramekin -- ...`) stay last, so a per-run flag still
overrides the profile's. The two builtin trivial profiles (`pi`, `claude`)
carry an empty `args`, leaving their behavior unchanged.

## The pi-bedrock profile

The profile lives in the user config (`~/.config/ramekin/config.kdl`),
because Bedrock credentials are machine-specific and do not belong in a
committed project config. It sets `--provider amazon-bedrock` with no
`--model`, leaving model choice to pi at run time. `AWS_PROFILE` and
`AWS_REGION` are placeholders for the user to fill in.

`~/.aws` mounts read-only. That works while the host SSO session is valid.
When the cached access token expires, the SDK's silent refresh cannot write
back to `~/.aws/sso/cache/`, so the user re-runs `aws sso login` on the host.
Read-only is the deliberate default; it keeps the credentials directory
unwritable from inside the container, consistent with ramekin's config
policy.

## Scope

In scope: the `args` field (parsing, the `Profile` struct, command
assembly), the builtin profiles' empty args, and the `pi-bedrock` user
profile.

Out of scope: mounting a `settings.json` fragment, any change to how CLI
trailing args are parsed, and Claude-specific Bedrock support (Claude uses
`CLAUDE_CODE_USE_BEDROCK`, an env var, which the existing `env` field already
covers).

## Testing

- Parse a profile with `args`, asserting the entries land in order.
- Parse multiple `args` nodes in one profile, asserting concatenation.
- Reject a non-string `args` entry.
- Assert command assembly places profile args between the prompt flag and the
  CLI trailing args.
- Assert the builtin profiles carry empty args.
