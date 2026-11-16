# CBGPU LaTeX

Real Overleaf Community Edition 6.3.2, with CBGPU identity, ZIP import and an independently implemented compile broker. This is not Overleaf Server Pro and does not enable its commercial modules.

- Researcher/app entry: `/latex`; real editor: `https://latex.cbgpu.com`.
- CBGPU approved-account login. Separate short-lived browser handoff, host-only Secure/HttpOnly cookies, native auth-session/approval/ban revalidation and live WebSocket revocation.
- ZIP: no fixed compressed, expanded-total or per-member byte cap; authenticated 8 MiB chunk transport, disk-backed validation/repacking, 1,000 files. Reject encryption, broken CRC, traversal, symlinks, duplicate names and excessive compression. Strip common directory wrappers, retain actual source structure, explicitly choose the main TeX file and compiler before importing. Failed validation never modifies existing projects. An ambiguous main document always remains selectable.
- Owner trash controls mirror Overleaf trash state; returning to the portal refreshes the project list. Other users cannot trash an owner’s project through this adapter.
- Engines: pdfLaTeX/XeLaTeX/LuaLaTeX/LaTeX, full TeX Live 2026 packages, no forced school or journal template. UTF-8 `.tex` inputs; convert legacy encodings before import.
- Per compile: 300 seconds, 2 CPU, 2 GiB RAM, 128 processes, disconnected networking, read-only container image, no capabilities, no host credentials or Docker socket. Compiler working files live in a **512 MiB tmpfs**, not an unbounded writable host mount. At most three simultaneous compiles. Root source snapshot is read-only; only validated regular output files are atomically published back. TeX/latexmk auxiliary subprocesses remain inside this sandbox. Oversized projects need splitting/asset reduction. This is CBGPU's own isolation adapter, not an official Overleaf CE sandbox feature.
- Native CE project authorization plus gateway checks protect direct output/PDF routes (some are served by CE nginx without app middleware).
- Project files/Mongo/Redis/secrets persist under `backend/private/latex`, never in Git. Compiler containers are disposable; they do not contain durable project data.

## Services

`docker compose -f backend/latex/compose.yml up -d` starts Mongo, Redis, CE and the private bridge. `cbgpu-latex-gateway.service` binds 127.0.0.1:3520; CE and trusted adapter are loopback 3521/3523. Only 3520 is published through the tunnel. Mongo/Redis have no host ports. `cbgpu-latex-compiler.service` exposes a narrow Unix socket to CLSI; CE never receives the Docker socket.

The maintained gateway, ZIP validator, compile broker and maintenance commands
are native Rust. Build with `cargo build --locked --release`; TypeScript CE
adapters still require `npm ci && npm run check && npm run build`. Mongo 8.0.34
uses `GLIBC_TUNABLES=glibc.pthread.rseq=1` on this kernel; retain the pinned digest.

The native candidate does not change running services. Configure
`CBGPU_LATEX_SOURCE`, `CBGPU_LATEX_PRIVATE` and a private
`CBGPU_LATEX_DATABASE_URL` in `~/.config/cbgpu/latex-rust.env` (mode 600). Prefer a
dedicated login inheriting the migration's `cbgpu_latex` role; it only accesses
private gateway state and the restricted fresh-session authorization function.
Never expose this URL or the CE bridge key to the browser. `CBGPU_LATEX_PORT`
can select an unused loopback candidate port; the default is 3520.

Before a separately approved cutover, back up the existing stack, validate
`migrations/20261116121608_latex_native_gateway_state.sql` through the backend
migration workflow, and run the native `import-legacy --apply` against a backup
copy first. It reads SQLite in read-only mode and inserts existing IDs/cookies
without overwriting newer PostgreSQL state. Repeating the import is safe.
`migrate --apply` exists for isolated fixtures/explicit maintenance and is never
run on service startup. No account migration, rollover or project import runs
implicitly. Keep Mongo/Redis/CE volumes, secrets and immutable live release in
place for rollback. Switch gateway/compiler units only after comparing counts,
native/CE session behavior, project ownership and sandbox smoke results.

CLSI runs as www-data; retain the existing inherited ACL for the broker UID.
For a fresh installation, native `install --apply` preserves secrets and uses
libacl directly. If the directory requires owner/root permission, explicitly run
the native `acl --apply --uid=HOST_BROKER_UID` with appropriate OS privileges,
then rerun installation as the service user. Never make the private parent
directory public. Startup refuses to replace an active compiler socket.

Account V2 remains **off** unless explicitly enabled after its approved rollout
(`CBGPU_ACCOUNT_V2=1`, with the frontend flag coordinated separately). With V2,
EXTERNAL/reverification users retain owner/member project read, edit, trash and
export access. Actual compilation requires the fresh backend `school_compute`
oracle; absent oracle fails closed. The pinned CE normal compile endpoint is
gated including case/trailing-slash variants, while private and diagnostic
compile routes and token-sharing grants are blocked. CE native ownership remains
enforced. Browser cookies/Authorization are stripped, only the mapped CE cookie
is injected, upstream Set-Cookie is discarded, and websockets revalidate native
identity every 20 seconds. With V2 off, existing approved-account semantics remain.

Imports have no fixed compressed/member/expanded-byte cap. Normalization is
disk-backed, TeX inspection streams UTF-8, and free-disk headroom is retained
(default 1 GiB, configurable using `CBGPU_LATEX_DISK_RESERVE_BYTES`). The 1,000-file,
unsafe-path, duplicate, symlink, encrypted/CRC and compression-bomb defenses
remain. Transport chunks are 8 MiB, not a project-size limit.

## Backups / verification

`cbgpu-latex-backup.timer` makes private daily Mongo(oplog)/Redis/files/config snapshots and retains seven. Compile/cache files are excluded. Backups are stored **on this computer**; they do not protect against loss of its disk. For recovery, restore to an empty stack and retain the original secrets; see `RESTORE.txt` inside each backup.

- `cargo test --locked`
- `cargo test --locked --test gateway_postgres -- --ignored`
- `cargo test --locked --test compiler_sandbox -- --ignored`
- `node backend/scripts/verify-latex.mjs` (temporary native/Overleaf users; no real mail)
- `CBGPU_TEST_PUBLIC=1 CBGPU_TEST_FRONTEND_PORT=3000 node backend/scripts/verify-latex.mjs`

`backup --apply` stores private Mongo(oplog), Redis, files/config and
`latex_gateway` PostgreSQL binary COPY snapshots with schema; it also snapshots legacy SQLite when
present. Native SQLite backup handles WAL safely. Retention remains seven; a
backup lock prevents concurrent pruning. `verify-restore --apply` restores only
into its own disconnected disposable Mongo instance. It does not restore the
live service. Native PostgreSQL backup does not require a host `pg_dump`. After restoring the
core database and applying the saved gateway schema, `restore-state --apply
--backup=/absolute/gateway-postgres.copy` merges transient records in one
transaction without overwriting existing IDs. Corrupt snapshots roll back.
Maintenance defaults to a plan until `--apply` is supplied.

## Source / license

Overleaf CE is AGPL-3.0: https://github.com/overleaf/overleaf. The pinned official image contains its corresponding application source under `/overleaf`. Retrieve the exact version with `docker pull sharelatex/sharelatex@sha256:ada39211c54a1e748065834ec44d3a49094cc475c28d8b803a251901592a94b2` and `docker cp CONTAINER:/overleaf ./overleaf-source`. CBGPU's compile adapter replacing `services/clsi/app/js/LocalCommandRunner.js`, bridge and integration source are provided at `/cbgpu/source.zip`, together with these run instructions. No proprietary Pro code is included.
