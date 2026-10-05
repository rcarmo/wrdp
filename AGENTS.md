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

- Canonical project name: `wrdp`. `scripts/project-tmp.sh` resolves scratch once,
  before Make changes child `TMPDIR`: a validated absolute `PROJECT_TMP_ROOT`
  ending in `wrdp`, else writable `/workspace/tmp/wrdp`, `$RUNNER_TEMP/wrdp`,
  the original `$TMPDIR/wrdp`, then the platform temp root `/tmp/wrdp` on supported
  Linux hosts. Invalid explicit overrides fail; no home-cache or ad-hoc fallback.
  The resolver rejects unsafe symlink/ownership paths and preserves `/workspace`
  as the operator-provided mount alias. No dependency on a host shared Makefile.
- Make exports actual tool settings, including resolved `PROJECT_TMP_ROOT` for
  recursive invocations. Direct commands must use the same environment.
  `make paths` prints it; `make prepare-paths` creates it without building.
- Downloads and caches use `cache/<tool>/`: Cargo `CARGO_HOME`/`CARGO_HOME_DIR`,
  Python `PYTHONPYCACHEPREFIX` and `PIP_CACHE_DIR`, `XDG_CACHE_HOME`, `CCACHE_DIR`,
  `SCCACHE_DIR`, `BUN_INSTALL_CACHE_DIR`, and `npm_config_cache`.
- Build output uses `build/<checkout-name>/{cargo-target,compositor,ground-init}`.
  Set distinct `WRDP_BUILD_ID` values for worktrees with the same basename.
- Scratch uses `runs/make/<timestamp-pid>/tmp`. `TMPDIR`, `TMP` and `TEMP` agree;
  recursive Make and subprocesses inherit `WRDP_RUN_ID`. Tests retain isolated,
  owned temporary directories below that run root, never real session state.
- Hosted CI explicitly sets `PROJECT_TMP_ROOT=$RUNNER_TEMP/wrdp` and calls the
  repository-owned `scripts/ci-paths.sh`, preserving the hierarchy and exporting
  actual tool variables via `GITHUB_ENV`. Overrides use the same validation.
- Installed `RUSTUP_HOME_DIR` and `RUSTUP_BIN` are durable toolchains, separate
  from the disposable Cargo registry/git cache. Do not relocate or delete them.
- Retained profiles, logs and receipts belong in `.local/evidence/` or their
  publication destination. Async PID/status/log-pointer files use
  `runs/async/<checkout>`; the logs themselves stay in `.local/evidence/async`.
- Cleanup validates paths and rejects symlinks, foreign ownership and active
  async PIDs. It never stops jobs or removes caches, evidence, other projects,
  installed toolchains or legacy paths. Stop all direct and async jobs first:
  the PID guard cannot detect arbitrary externally launched processes. Preserve
  required evidence before manually removing any old disposable directories.
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
