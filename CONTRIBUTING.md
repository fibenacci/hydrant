# Contributing to hydrant

hydrant is a source-agnostic ingest service: it accepts records from any external system
over one canonical HTTP protocol and republishes an explicitly allow-listed subset through a
cacheable public read API. `crates/core` exists; the store, the HTTP surface and the CLI do not
yet.

Read the invariants below before proposing anything structural. They are not style
preferences; each one exists because its absence produced a class of silent-data-loss or
data-leak bug in a domain-aware replication system the author maintains.

## Code of conduct

Participation is governed by the [Contributor Covenant](CODE_OF_CONDUCT.md).

## Invariants

Code that violates one of these is wrong even if it passes every test.

1. **The service never knows the source domain.** No per-entity handlers, no foreign-key
   resolution, no dependency ordering between collection types. References between records
   are opaque IDs inside JSON; resolving them is the consumer's job. Ordering entity types so
   that foreign keys resolve fails silently — one inverted edge once dropped ~19,000 rows in
   production while reporting success. A document store with no FK resolution cannot express
   that bug; keep it that way.
2. **Projection happens at ingest, never at read.** Fields outside a collection's allow-list
   are dropped before persistence. Filtering on read makes every bug in the read path a data
   leak; filtering on write makes a bug a missing field.
3. **Deny by default, with no wildcard.** An unknown key is dropped and counted in
   `ingest_dropped_field_total{collection,field}`, so a source system adding a field shows up
   in monitoring instead of in a response. `type: object` without an `allow` list is a schema
   error, not a pass-through.
4. **Canonicalization is a wire contract.** RFC 8785 (JCS) + SHA-256, everywhere a hash is
   computed. Never hand-roll a canonical form and never change the existing one — a change
   makes every collection report drift simultaneously and forces a full re-export.
5. **Ingest is idempotent over `content_hash`.** An identical payload advances no `seq` and
   emits no change-feed entry. This is what allows senders to keep their retry logic trivial.
6. **Translation belongs at the sender.** One canonical ingest format. The moment the service
   learns to translate source shapes, it grows with every new source system.
7. **`source` is a partitioning label, not a security boundary.** Everything in the store is
   public. Never build isolation on it.

## Workspace layout

```
crates/core/     domain types, JCS hashing, projection engine — MUST stay free of I/O deps
crates/store/    trait Store + PostgreSQL implementation
crates/api/      axum routers: public + ingest
crates/server/   binary, config, telemetry
crates/cli/      schema validate | drift check | reproject | backfill
schemas/         example collection definitions
sdk/php/         sender SDK
```

`core` having no I/O dependencies is a design constraint, not an accident: it keeps
projection and hashing pure and property-testable, and lets the CLI use them without a
database. Do not add `sqlx`, `axum` or `tokio` to it.

## Running locally

Three prerequisites, all cross-platform:

- **Rust** via [rustup](https://rustup.rs/). The toolchain itself is pinned in
  `rust-toolchain.toml`, so `cargo` picks the right one on first invocation.
- **A container runtime that reads the Compose specification** — Docker Desktop, Podman,
  Colima, Rancher Desktop. The image is pinned to a multi-architecture index digest, so the
  same `compose.yaml` runs natively on arm64 macOS, amd64 Linux and Windows/WSL2 without a
  platform override.
- **GNU make**, which drives every task. On Windows use WSL2 or Git Bash; the container
  runtime can stay on the Windows side.

```bash
cp .env.example .env
make db-up            # PostgreSQL, waits until it accepts connections
make db-shell         # psql inside the container
make db-down          # stop and drop the volume
```

The database listens on **5433** by default, not 5432, because that port is usually taken by
another project's container or a native install. Override `HYDRANT_DB_PORT` and `DATABASE_URL`
together if you want a different one.

For Podman or any other runtime, override the compose command rather than editing the file:

```bash
make db-up COMPOSE="podman compose"
```

Then start the service:

```bash
make run          # applies migrations, serves on 127.0.0.1:8080 by default
curl -s localhost:8080/health
```

To put something in, mint a credential and push a batch. The token is printed once and is not
recoverable, because only its HMAC is stored:

```bash
cargo run -p hydrant-server -- token mint --source sap-stage --label "local test"

curl -X POST localhost:8080/v1/ingest/catalog.product \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '[{"op":"upsert","id":"SW1","payload":{"sku":"SW-1","price":9.99,"secret":"dropped"}}]'

curl -s localhost:8080/v1/sap-stage/catalog.product/SW1
```

The response to the push names every field the schema did not release, and the read shows that
those fields were never stored. Every setting comes from the environment — see `.env.example`;
there is no config file, because a file would be a second place to look when a setting is wrong.

## Database and query metadata

`crates/store` is tested against a real PostgreSQL rather than a mock. Idempotency lives in an
`ON CONFLICT ... WHERE` clause and the tombstone rule lives in a check constraint; neither can be
verified anywhere else. So `make db-up` is a prerequisite for `make test`.

Queries are checked at compile time against the metadata in `.sqlx`, which is committed. That is
what lets every CI job except the test job build without a database, and it makes a query change
visible in review rather than hidden inside a string. When you add or change a query:

```bash
make db-up
make migrate
make sqlx-prepare     # rewrites .sqlx
```

Commit the `.sqlx` change together with the query. CI runs `make sqlx-check` and fails if the two
have drifted. `make ci` runs that check too when `sqlx-cli` is installed, and says so when it is not
— installing it is optional:

```bash
cargo install sqlx-cli --version 0.8.6 --no-default-features --features postgres,rustls
```

The version is pinned deliberately: a newer `sqlx-cli` writes metadata the pinned `sqlx` library
cannot read. `sqlx` itself is held at 0.8 because 0.9 requires Rust 1.94, well beyond the 1.88 MSRV
declared in `Cargo.toml`.

## Local checks

Reproduce the CI pipeline before pushing:

```bash
make ci          # fmt + clippy + MSRV check + docs + deny + tests + doctests
```

While iterating, scope it to what you touched — `cargo clippy -p hydrant-core`,
`cargo test -p hydrant-core`. The full workspace run belongs once, before you open the PR.

**Never pipe a gate whose result you intend to trust.** A pipeline reports the exit status of
its *last* command, and most shells have no `pipefail` by default, so `make ci | tail` turns a
red gate green. Run it bare, or propagate the status explicitly:

```bash
set -o pipefail; make ci 2>&1 | tail -40
```

## Pull requests

- One topic per PR. Split by type — a fix, a refactor and a test-only change are three PRs.
- Public API surface changes are documented in the same change, not afterwards.
- **Schema changes deserve a real review.** Adding a field to a collection's allow-list makes
  that field public; that is the entire point of keeping schemas in git rather than in an
  admin UI. Say in the PR description what the new field contains and why it may be released.
- New abstractions need a concrete second implementation to justify them. Prefer the change
  that shrinks the codebase.

## Commit convention

[Conventional Commits](https://www.conventionalcommits.org/), in English, imperative mood,
lower-case subject, no trailing period, subject at most 72 characters:

```
<type>(<scope>)<!>: <subject>
```

- `<scope>` is optional — usually the crate or area (`core`, `store`, `api`, `schema`).
- A trailing `!` (or a `BREAKING CHANGE:` footer) marks a breaking change and triggers a major
  version bump.
- One type per commit. A fix, a refactor and a test-only change are three commits.

| Type       | Use for                                   |
|------------|-------------------------------------------|
| `feat`     | a new feature                             |
| `fix`      | a bug fix                                 |
| `perf`     | a performance improvement                 |
| `refactor` | code change that neither fixes nor adds   |
| `docs`     | documentation only                        |
| `test`     | adding or fixing tests                    |
| `build`    | build system or dependencies              |
| `ci`       | CI configuration                          |
| `style`    | formatting / style, no logic change       |
| `chore`    | tooling / housekeeping                    |
| `revert`   | reverting a previous commit               |

Examples:

```
feat(core): add RFC 8785 canonicalization and content hashing
fix(api): derive the feed validator from max(seq), not from now()
refactor(store): fold the digest query into the cursor scan
docs: state why projection never happens on read
```

The changelog groups entries by type automatically; the mapping lives in `release-plz.toml`.

## Releases

[release-plz](https://release-plz.dev/) runs as a job in the CI workflow on every push to
`main`, after the gates pass. It opens a release PR that bumps the version and updates
`CHANGELOG.md` from the commits; merging that PR pushes the `v<version>` tag and creates the
GitHub release. Publishing crates to crates.io is deliberately off — see `release-plz.toml`.

## Repository settings

For anyone forking this or setting up a mirror, the configuration the workflows assume:

- **`CI` is the only required status check.** It aggregates every job and fails on
  cancelled or skipped ones, so a green tick cannot mean "did not run".
- **No direct pushes to `main`**, linear history, and a required review — the release PR is
  the one exception release-plz needs write access for.
- **Signed commits.** Every commit in this repository is GPG-signed.
- **`RELEASE_PLZ_TOKEN`** (a PAT or GitHub App token) as a repository secret. Without it the
  release PR is opened with the default token and, by GitHub's recursion guard, never runs CI.
- Actions are pinned to commit SHAs; Dependabot updates them weekly together with the cargo
  dependencies. Do not replace a pin with a floating tag.

## License

By contributing you agree that your contribution is licensed under
[Apache-2.0](LICENSE), per §5 of the license itself. There is no CLA.
