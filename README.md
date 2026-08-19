# hydrant

**A source-agnostic ingest service that publishes data read-only, public by design.**

Point any system at it — an ERP, a PIM, a CMS, a Symfony monolith, a scraper — push
records in over a single canonical HTTP protocol, and get back a cacheable public read
API with a change feed, ETags, and an OpenAPI document derived from your own schemas.

The service never knows your domain. It stores documents, not entities. There are no
per-entity handlers, no foreign-key resolution, no ORM on the receiving side — which is
precisely what makes it work for any source system.

## Public by design

This is not "an API without authentication". It is a storage rule:

> There must be no code path on which unreleased data can reach a response — because it
> was never stored in the first place.

Every record passes a declarative allow-list **at ingest time**. Fields not named in the
collection schema are dropped before persistence. Filtering on read makes every bug a
data leak; filtering on write makes a bug a missing field.

Consequences, all deliberate:

- **Deny by default.** A new field appearing in the source system never becomes public
  automatically. Unknown keys are dropped and counted in a metric so the omission is visible.
- **Schemas live in git, not in an admin UI.** Releasing a field is a pull request with a
  review, not a checkbox.
- **Raw payload retention is opt-in and off by default.** It is the only place unfiltered
  data would sit at rest.

## Status

Design stage — nothing is implemented yet. The architecture is settled; the design notes
themselves stay in the author's working directory until there is code they describe.
What the service guarantees is in [CONTRIBUTING.md](CONTRIBUTING.md#invariants); how it is
built is in the [Makefile](Makefile).

## Not to be confused with

PostgREST, Supabase, Directus and friends expose a database you already have. This runs
the other direction: it *accepts* data from a foreign system and republishes an explicitly
allow-listed subset. Different direction, different threat model.

## Contributing

The invariants, the workspace layout, the local gates and the commit convention are in
[CONTRIBUTING.md](CONTRIBUTING.md). What counts as a vulnerability in a service that is
public on purpose — and what explicitly does not — is in [SECURITY.md](SECURITY.md).
Participation is governed by the [Contributor Covenant](CODE_OF_CONDUCT.md).

## License

[Apache-2.0](LICENSE). Copyright 2026 Benjamin Letzel.
