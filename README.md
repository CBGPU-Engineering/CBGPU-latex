# CBGPU LaTeX

[![CI](https://github.com/CBGPU-Engineering/CBGPU-latex/actions/workflows/ci.yml/badge.svg)](https://github.com/CBGPU-Engineering/CBGPU-latex/actions/workflows/ci.yml)
[![License: AGPL 3.0](https://img.shields.io/badge/License-AGPL%203.0-blue)](LICENSE)

Rust/Axum Overleaf Community Edition integration for CBGPU: authenticated project access,
ZIP import, main-document/compiler selection, trash synchronization and a
separate resource-limited TeX compile broker. This is not Overleaf Server Pro.

## Tests

```bash
git clone https://github.com/CBGPU-Engineering/CBGPU-latex.git
cd CBGPU-latex
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
npm ci
npm run check
npm run build
```

ZIP/security tests use synthetic local files, including archives above the old
50 MB/150 MB limits. `cargo test --locked --test gateway_postgres -- --ignored`
uses its own disconnected PostgreSQL container and fake loopback editor. The
real compile isolation test additionally requires the reviewed pinned TeX image:
`cargo test --locked --test compiler_sandbox -- --ignored`. Neither suite mounts
existing project volumes or connects to the production database.

`cargo test --locked --test school_http_stage -- --ignored` additionally checks
fresh synthetic School B sessions against an explicitly prepared isolated Auth
clone. Set `CBGPU_LATEX_TEST_STAGE_DSN_FILE` and
`CBGPU_LATEX_TEST_STAGE_ACTORS_FILE` to private fixture files; the test refuses
production addresses and uses a restricted gateway role. It reads no project
state and verifies that school membership does not grant CBsh compile capacity.

## Deployment

[CBGPU-backend](https://github.com/CBGPU-Engineering/CBGPU-backend) pins this repository as its `latex/`
submodule. [CBGPU-platform](https://github.com/CBGPU-Engineering/CBGPU-platform) assembles the full layout,
placing it at `backend/latex`. Existing setup scripts expect that integration
layout and private state under `backend/private/latex`.

Read [integration, compile limits, source offer and backup details](docs/INTEGRATION.md)
before deploying. Do not copy private project files or credentials into Git.

The production gateway, compile broker and backup service run native Rust after
a verified, idle handoff on 2026-11-17. Source checkout/install commands never
automatically migrate an existing service.
Build with `cargo build --locked --release`. Its `gateway` and `compiler`
commands replace the old service processes; maintenance commands default to a
plan and require `--apply`. Follow the reviewed handoff/rollback procedure in
the integration guide, including backup, PostgreSQL authorization and quiescent
legacy-state import. Existing
Overleaf/Mongo project IDs, owners, files and memberships are never reset.

## License

This integration retains [AGPL-3.0](LICENSE). Overleaf CE and TeX packages retain
their own licenses; keep the corresponding source offer and notices. It is not
relicensed by the Apache-2.0 license used for other CBGPU components.


Maintained Node.js adapters and the editor integration use TypeScript. Run
`npm ci`, `npm run check` and `npm run build` before starting Compose. The pinned
Overleaf image loads the generated `dist/` adapters at its original module paths.
Generated JavaScript is excluded from source control.


Scoped identity accepts server `CBGPU_ACCOUNT_V2` and `CBGPU_MULTISCHOOL_V1`:
unset, empty, `0`, and `false` preserve legacy mode; `1` or `true` on either
select scoped identity. Whitespace, case variants and other values stop startup,
even when the other flag is enabled. Project identity remains distinct from
compute permission. Suspension, expired sessions and account bans deny gateway
identity in every mode without deleting project or transient upload records.
