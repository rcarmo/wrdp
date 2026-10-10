<!-- RUI-PROFILE-LIFECYCLE-20261005 -->
## Current profiling and cleanup rule — supersedes older text below

Rui's explicit rule: **profile and tune during pre-release tests; remove profiling data immediately after analysis/use.** Ordinary development tests do not require profiling on every run. Targeted diagnostic profiling is optional when useful and follows the same disposal rule.

- During pre-release verification, capture CPU and heap/allocation behaviour, analyse hotspots and tune avoidable allocations/repeated work. Compare equivalent workloads without weakening correctness/security/numerical contracts. For Go, inspect CPU, alloc_space and alloc_objects; coverage alone is not profiling.
- Keep captures and matching artifacts only while the current analysis needs them. Once used, immediately delete raw profiles, traces, matching test binaries, temporary fixtures and disposable logs, including failed/probe artifacts after diagnosis. Retain only concise conclusions and important measurements/limitations. Do not keep indefinite raw archives or copy them into exports/reports/evidence to evade cleanup.
- Remove all completed disposable cache/build/test/run data promptly. Never delete files still in use: finish or safely pause the owning job and clean at a safe boundary. Preserve source, installed toolchains, durable datasets/checkpoints and intentional release assets. Minimise disk usage; no random exports or redundant snapshots.
- This rule overrides **every conflicting older paragraph in this file and linked local guidance**, including “profile every test”, “unprofiled tests prohibited”, “retain all raw evidence”, “never delete profiles” and cleanup exclusions based only on an evidence/profiles directory name. Update helper/CI cleanup behaviour accordingly; do not weaken pre-release analysis.
- Existing no-agent-contact and execution-pause rules remain unchanged. This policy grants no unsolicited coordination or automatic job restart.
<!-- /RUI-PROFILE-LIFECYCLE-20261005 -->



# Repository Guidance for Agents

## Local Overrides

If `.agents.local.md` exists, read it before making changes. It contains machine- or workspace-specific instructions and is ignored by Git; never commit its contents.

## Engineering

- Prefer simple, local changes and apply YAGNI.
- Keep refactors mechanical before changing behavior.
- Preserve protocol behavior unless a task changes it explicitly.
- Keep RDP server, session, channel, desktop, service, security, and runtime ownership separate.
- Read relevant files before editing and validate with the Makefile.

## Documentation And Comments

- Treat documentation and comments as source code: review and rewrite them when moving code.
- Explain protocol constraints, ownership, invariants, negotiated capabilities, client quirks, and fallback reasons.
- Reference durable specifications or protocol names where known: MS-RDPBCGR, MS-RDPEGFX, MS-RDPECLIP, MS-RDPEA, Wayland, PipeWire, XDG Desktop Portal.
- Do not narrate obvious code, refactor history, private process, or repository preparation.
- Avoid generated prose: abstract value claims, generic capability brochures, repeated assurances, and headings such as “Overview” or “Conclusion”.
- Put consequential design choices in `docs/decisions/`.

## Rust

- Prefer concrete types until there are two real implementations or a useful test seam.
- Keep `unsafe` small, documented, and tied to a stated invariant.
- Use `Result` for recoverable errors; avoid `unwrap()` and `expect()` in production paths.
- Keep feature combinations used by the Makefile compiling.

## Development Paths

- Canonical project name: `wrdp`. `scripts/project-tmp.sh` resolves scratch before
  Make or CI redirects child temp variables. An absolute `PROJECT_TMP_BASE`
  selects `<base>/wrdp`; compatible `PROJECT_TMP_ROOT` selects an absolute path
  ending in `wrdp`. If both are supplied they must agree. Empty, invalid or
  unusable overrides fail rather than falling back.
- Without overrides, CI selects usable `$RUNNER_TEMP`, then the original inherited
  `$TMPDIR`, then platform temp (`/tmp` on supported Linux hosts), even when
  `/workspace/tmp` exists. Local invocations prefer usable `/workspace/tmp`, then
  platform temp; they ignore runner and inherited temp hints. Always append
  `wrdp`. The resolver rejects unsafe symlink/ownership paths, retaining
  `/workspace` as the operator-provided mount alias. No host helper is required.
- `PROJECT_ORIGINAL_TMPDIR` snapshots incoming `TMPDIR` before redirection. Make
  and CI export the snapshot and resolved `PROJECT_TMP_ROOT`; recursive children
  reuse that root without appending another project name. Direct commands must
  use the same environment. `make paths` prints it; `make prepare-paths` creates
  it without building. Generic layout: `cache`, `build`, `tests`, `logs`, `runs`.
- Downloads and caches use `cache/<tool>/`: Cargo `CARGO_HOME`/`CARGO_HOME_DIR`,
  Python `PYTHONPYCACHEPREFIX` and `PIP_CACHE_DIR`, `XDG_CACHE_HOME`, `CCACHE_DIR`,
  `SCCACHE_DIR`, `BUN_INSTALL_CACHE_DIR`, and `npm_config_cache`.
- Build output uses `build/<checkout-name>/{cargo-target,compositor,ground-init}`.
  Set distinct `WRDP_BUILD_ID` values for worktrees with the same basename.
- Scratch uses `runs/make/<timestamp-pid>/tmp`. `TMPDIR`, `TMP` and `TEMP` agree;
  recursive Make and subprocesses inherit `WRDP_RUN_ID`. Tests retain isolated,
  owned temporary directories below that run root, never real session state.
- Hosted CI explicitly sets `PROJECT_TMP_BASE=$RUNNER_TEMP` and calls the
  repository-owned `scripts/ci-paths.sh`, preserving the hierarchy and exporting
  the snapshot, resolved root and tool variables via `GITHUB_ENV`. Overrides use
  the same validation. `TEST_ROOT=.../tests` and `LOG_ROOT=.../logs` are disposable
  scratch; raw logs/profiles in any path are deleted after analysis.
- Installed `RUSTUP_HOME_DIR` and `RUSTUP_BIN` are durable toolchains, separate
  from the disposable Cargo registry/git cache. Do not relocate or delete them.
- Keep only concise conclusions and intentional release receipts in .local/evidence; raw profiles and disposable logs must be deleted after analysis. Async job status/locks remain intact while active.

- Cleanup validates ownership/symlinks and checks direct/async active jobs. Delete completed disposable cache/build/run/profile/log data immediately after use; keep concise conclusions only. Never stop jobs merely to clean, remove active files, installed toolchains, source, durable assets or another project. PID guards alone cannot detect arbitrary externally launched processes.
- Installed `/run/user/<uid>/wrdp`, user home/config/Downloads and wallpaper
  caches are product runtime state, not development output. Keep their existing
  ownership and atomic-update rules. User provisioning gives child processes a
  separate target-user-owned temporary directory below the current run root.
- Vendored ground-init build actions require an explicit `root` or
  `GROUND_INIT_BUILD_ROOT`; WRDP Make supplies the project build path. There is
  no implicit home-directory build fallback.

## Commits

- Configure commits as `Rui Carmo <rui.carmo@gmail.com>`.
- Keep commits focused by architecture boundary.
- Run `git diff --check` before committing.
- Do not commit build output, logs, credentials, local paths, or local override files.
