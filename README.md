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

## Deployment

[CBGPU-backend](https://github.com/CBGPU-Engineering/CBGPU-backend) pins this repository as its `latex/`
submodule. [CBGPU-platform](https://github.com/CBGPU-Engineering/CBGPU-platform) assembles the full layout,
placing it at `backend/latex`. Existing setup scripts expect that integration
layout and private state under `backend/private/latex`.

Read [integration, compile limits, source offer and backup details](docs/INTEGRATION.md)
before deploying. Do not copy private project files or credentials into Git.

The native service is a **candidate**, not an automatic production cutover.
Build with `cargo build --locked --release`. Its `gateway` and `compiler`
commands replace the old service processes; maintenance commands default to a
plan and require `--apply`. Back up and validate the private PostgreSQL migration
and read-only legacy session import before switching any installed unit. Existing
Overleaf/Mongo project IDs, owners, files and memberships are never reset.

## License

This integration retains [AGPL-3.0](LICENSE). Overleaf CE and TeX packages retain
their own licenses; keep the corresponding source offer and notices. It is not
relicensed by the Apache-2.0 license used for other CBGPU components.


Maintained Node.js adapters and the editor integration use TypeScript. Run
`npm ci`, `npm run check` and `npm run build` before starting Compose. The pinned
Overleaf image loads the generated `dist/` adapters at its original module paths.
Generated JavaScript is excluded from source control.
