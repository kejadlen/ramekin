# Profile-carried agent args implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a ramekin profile pin CLI flags for the agent binary, so a `pi-bedrock` profile can select `--provider amazon-bedrock` on its own.

**Architecture:** Add an `args: Vec<String>` field to `Profile`, parsed from one or more `args` nodes in a profile's KDL block. Thread the active profile's args into the generated compose command, placed after the prompt flag and before the per-run CLI trailing args so the latter still override.

**Tech Stack:** Rust (edition 2024), `kdl` crate for parsing, `serde_yaml` for compose generation, `miette` for errors.

## Global Constraints

- Rust edition 2024. Error handling uses `miette` (`bail!`, `miette!`, `.into_diagnostic()`); no `.unwrap()` in non-test code.
- File I/O uses `fs-err`, never `std::fs` (clippy enforces this).
- All CI checks must pass: `cargo fmt --all --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`. Run `just` before committing.
- VCS is jj, not git. Each logical change is its own commit (`jj commit -m '...'`). Commit subjects are plain English, capitalized, under 60 chars, no conventional-commit prefixes. AI-drafted messages carry an `Assisted-by: Claude Opus 4.8 via pi` trailer.
- Unknown KDL nodes and fields fail loudly (`bail!`).

---

### Task 1: Add the `args` field to Profile and its KDL parsing

**Files:**
- Modify: `src/config.rs` — `Profile` struct, `Profile::builtin`, `parse_profile`, new `parse_args` helper, the `scoped` test helper
- Test: `src/config.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `Profile.args: Vec<String>` (public field on the existing `pub struct Profile`); `fn parse_args(node: &KdlNode) -> Result<Vec<String>>` (private).

- [ ] **Step 1: Write the failing tests**

Add these three tests inside the `tests` module in `src/config.rs` (near the existing `parse_profile_definition` test):

```rust
#[test]
fn parse_profile_with_args() {
    let raw = parse_config(
        r#"
        profile "pi-bedrock" {
            agent "pi"
            args "--provider" "amazon-bedrock"
        }
        "#,
    )
    .unwrap();
    let p = &raw.profiles[0];
    assert_eq!(p.args, vec!["--provider", "amazon-bedrock"]);
}

#[test]
fn parse_profile_concatenates_multiple_args_nodes() {
    let raw = parse_config(
        r#"
        profile "x" {
            agent "pi"
            args "--provider" "amazon-bedrock"
            args "--model" "some-model"
        }
        "#,
    )
    .unwrap();
    assert_eq!(
        raw.profiles[0].args,
        vec!["--provider", "amazon-bedrock", "--model", "some-model"]
    );
}

#[test]
fn parse_profile_rejects_non_string_args() {
    let result = parse_config("profile \"x\" {\n    agent \"pi\"\n    args 42\n}");
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib parse_profile_with_args parse_profile_concatenates_multiple_args_nodes parse_profile_rejects_non_string_args`
Expected: compile error — `Profile` has no field `args` (and `parse_args` undefined).

- [ ] **Step 3: Add the `args` field to the struct and both literal constructors**

In `src/config.rs`, add the field to `Profile` (after `mounts`):

```rust
pub struct Profile {
    pub name: String,
    pub agent: Agent,
    pub env: Vec<EnvVar>,
    pub mounts: Vec<Mount>,
    pub args: Vec<String>,
}
```

In `Profile::builtin`, add `args: Vec::new()` to the constructed `Self { ... }`:

```rust
.map(|agent| Self {
    name: agent.name().to_string(),
    agent,
    env: Vec::new(),
    mounts: Vec::new(),
    args: Vec::new(),
})
```

In the `scoped` test helper's `Profile { ... }` literal, add `args: Vec::new()` after `mounts: Vec::new()`.

- [ ] **Step 4: Add the `parse_args` helper**

In `src/config.rs`, add next to `parse_mounts`/`parse_env`:

```rust
/// Parse an `args` node: bare string entries passed verbatim to the agent
/// binary (`args "--provider" "amazon-bedrock"`). No properties, no block.
fn parse_args(node: &KdlNode) -> Result<Vec<String>> {
    if node.children().is_some() {
        bail!("`args` takes inline string values (args \"--flag\" \"value\"), not a block");
    }
    node.entries()
        .iter()
        .map(|entry| {
            if entry.name().is_some() {
                bail!("`args` takes bare string values, not properties");
            }
            entry
                .value()
                .as_string()
                .map(str::to_string)
                .ok_or_else(|| miette!("`args` takes string values, got {}", entry.value()))
        })
        .collect()
}
```

- [ ] **Step 5: Wire `args` into `parse_profile`**

In `parse_profile`, add the accumulator and match arm, then include it in the returned `Profile`:

```rust
    let mut agent = None;
    let mut env = Vec::new();
    let mut mounts = Vec::new();
    let mut args = Vec::new();
    for child in children.nodes() {
        match child.name().value() {
            "agent" => agent = Some(Agent::parse(&single_string_arg(child)?)?),
            "env" => env.extend(parse_env(child)?),
            "mounts" => mounts.extend(parse_mounts(child)?),
            "args" => args.extend(parse_args(child)?),
            other => bail!("unknown `profile` field `{other}`"),
        }
    }

    Ok(ProfileNode::Definition(Profile {
        agent: agent
            .ok_or_else(|| miette!("profile `{name}` is missing `agent` (`pi` or `claude`)"))?,
        name,
        env,
        mounts,
        args,
    }))
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, including the three new tests and all pre-existing config tests.

- [ ] **Step 7: Commit**

```bash
just
jj commit -m 'config: let profiles carry agent CLI args

Add an `args` field to Profile, parsed from one or more `args` nodes in a
profile block, so a profile can pin flags the agent needs (pi selects
Bedrock via --provider, which has no env equivalent). Entries concatenate
like env/mounts blocks; args ride with the wholesale profile-merge rule.

Assisted-by: Claude Opus 4.8 via pi'
```

---

### Task 2: Thread profile args into the container command

**Files:**
- Modify: `src/main.rs` — `ComposeParams` struct, its destructure in `generate_compose`, the command assembly, the `run` call site, the `compose_params` test helper
- Test: `src/main.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Profile.args` from Task 1 (via `self.config.profile.args`).
- Produces: `ComposeParams.profile_args: &'a [String]`. Command layout becomes `[prompt_flag, PROMPT_TARGET] ++ profile_args ++ agent_args`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/main.rs` (near `generate_compose_long_form_binds`):

```rust
#[test]
fn generate_compose_places_profile_args_before_cli_args() {
    let profile_args = vec!["--provider".to_string(), "amazon-bedrock".to_string()];
    let cli_args = vec!["--model".to_string(), "override".to_string()];
    let mut params = compose_params(&[], &[]);
    params.profile_args = &profile_args;
    params.agent_args = &cli_args;
    let yaml = generate_compose(params);

    let prompt = yaml.find(PROMPT_TARGET).expect("prompt target missing");
    let provider = yaml.find("amazon-bedrock").expect("profile arg missing");
    let model = yaml.find("override").expect("cli arg missing");
    assert!(
        prompt < provider && provider < model,
        "expected prompt < profile arg < cli arg, got {yaml}"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib generate_compose_places_profile_args_before_cli_args`
Expected: compile error — `ComposeParams` has no field `profile_args`.

- [ ] **Step 3: Add the `profile_args` field to `ComposeParams`**

In `src/main.rs`, add it just before `agent_args` in the struct:

```rust
    prompt_flag: &'a str,
    /// CLI flags the active profile pins for the agent binary. Placed before
    /// the per-run trailing args so a `ramekin -- ...` flag still wins.
    profile_args: &'a [String],
    agent_args: &'a [String],
```

- [ ] **Step 4: Destructure and use it in `generate_compose`**

Add `profile_args,` to the `let ComposeParams { ... } = params;` destructure (before `agent_args,`). Then update the command assembly:

```rust
    // Always pass the prompt flag for the ramekin container context.
    // Profile args come next; user-supplied CLI args come last so they win.
    let command: Vec<String> = [prompt_flag.to_string(), PROMPT_TARGET.to_string()]
        .into_iter()
        .chain(profile_args.iter().cloned())
        .chain(agent_args.iter().cloned())
        .collect();
```

- [ ] **Step 5: Pass the active profile's args at the `run` call site**

In `run`, in the `generate_compose(ComposeParams { ... })` call, add `profile_args` just before `agent_args`:

```rust
            prompt_flag: match agent {
                config::Agent::Pi => "--append-system-prompt",
                config::Agent::Claude => "--append-system-prompt-file",
            },
            profile_args: &self.config.profile.args,
            agent_args,
```

- [ ] **Step 6: Update the `compose_params` test helper**

In the `compose_params` helper's returned `ComposeParams { ... }`, add `profile_args: &[],` before `agent_args: &[]`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, including the new test and all pre-existing main tests.

- [ ] **Step 8: Commit**

```bash
just
jj commit -m 'Run the active profile'"'"'s pinned args

generate_compose now inserts profile.args between the prompt flag and the
per-run CLI trailing args, so selecting a profile applies its flags while
`ramekin -- ...` can still override them.

Assisted-by: Claude Opus 4.8 via pi'
```

---

### Task 3: Document the `args` field

**Files:**
- Modify: `AGENTS.md` (repo root) — the KDL grammar sentence and the profile/merge description
- Modify: `docs/config-redesign.md` — the "A profile is a named bundle" sentence and example

**Interfaces:**
- Consumes: nothing (prose only).
- Produces: nothing.

- [ ] **Step 1: Update `AGENTS.md`**

In the architecture note describing the grammar, extend the profile clause to mention `args`. Find:

> and `profile` nodes (with children = definition, bare = selection). Unknown nodes fail loudly.

Replace with:

> and `profile` nodes (with children = definition carrying `agent`/`env`/`mounts`/`args`, bare = selection). Unknown nodes fail loudly.

In the merge-layers sentence, find `profile (the active profile's env/mounts)` and change to `profile (the active profile's env, mounts, and args)`.

- [ ] **Step 2: Update `docs/config-redesign.md`**

Find:

> A profile is a named bundle: agent, env vars, extra mounts.

Replace with:

> A profile is a named bundle: agent, env vars, extra mounts, and agent CLI args.

Then, after the `pi-glm` example block, add a sentence and example showing `args`:

````markdown
A profile also pins CLI flags for the agent binary through `args`, for
providers the agent selects by flag rather than by environment (pi reaches
Amazon Bedrock through `--provider`, which has no env equivalent):

```kdl
profile "pi-bedrock" {
    agent "pi"
    args "--provider" "amazon-bedrock"
    env {
        AWS_PROFILE
        AWS_REGION
    }
    mounts { "~/.aws" }
}
```

Args ride with the profile definition, so the wholesale profile-merge rule
covers them too; the run's trailing `ramekin -- ...` args come after and
override.
````

- [ ] **Step 3: Verify the docs read correctly**

Run: `rg -n "args" AGENTS.md docs/config-redesign.md`
Expected: the new mentions appear; skim them for accuracy.

- [ ] **Step 4: Commit**

```bash
jj commit -m 'docs: document profile args

Assisted-by: Claude Opus 4.8 via pi'
```

---

### Task 4: Add the pi-bedrock profile to the user config

**Files:**
- Modify: `~/.config/ramekin/config.kdl` (outside the repo — the machine's user config; not a ramekin-repo commit)

**Interfaces:**
- Consumes: the `args` field from Task 1 and its runtime wiring from Task 2.
- Produces: a selectable `pi-bedrock` profile.

Note: this file is a plain file on this machine (not a dotfiles symlink), so there is no separate repo to commit it to. This task is a config edit, not a version-control change.

- [ ] **Step 1: Append the profile**

Add to `~/.config/ramekin/config.kdl` (leave the existing `profile "claude"` selection as the default):

```kdl
// Pi against Amazon Bedrock. Pi selects the provider by flag (no env
// equivalent); the model is left to pi at run time. AWS creds come from an
// SSO profile — fill in the placeholders and `aws sso login` on the host.
// ~/.aws mounts read-only, so an expired SSO token means re-logging in on
// the host rather than a silent in-container refresh.
profile "pi-bedrock" {
    agent "pi"
    args "--provider" "amazon-bedrock"
    env {
        AWS_PROFILE "REPLACE_ME"
        AWS_REGION "REPLACE_ME"
    }
    mounts { "~/.aws" }
}
```

- [ ] **Step 2: Verify the config parses and the profile resolves**

Run: `ramekin -p pi-bedrock config`
Expected: no parse error; the resolved config shows profile `pi-bedrock`, agent `pi`, the `~/.aws` mount (read-only), and the `AWS_PROFILE`/`AWS_REGION` env vars. (It resolves even with `REPLACE_ME` values — those only matter at agent run time.)

---

## Self-Review

Spec coverage: the `args` field and parsing (Task 1), command assembly with profile args before CLI args (Task 2), builtin profiles' empty args (Task 1, `Profile::builtin`), the pi-bedrock user profile (Task 4), and docs (Task 3) all map to the spec. The spec's out-of-scope items (settings.json fragment, CLI-arg parsing changes, Claude Bedrock) are correctly untouched.

Placeholder scan: `REPLACE_ME` in Task 4 is an intentional, spec-mandated placeholder in the user's config, not a plan gap. All code steps show complete code.

Type consistency: `Profile.args: Vec<String>` (Task 1) is consumed as `&self.config.profile.args` into `ComposeParams.profile_args: &[String]` (Task 2) — types line up. `parse_args` returns `Result<Vec<String>>`, matching the `args.extend(...)` accumulator.
