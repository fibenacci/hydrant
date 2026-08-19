# Security Policy

## Reporting a vulnerability

**Do not open a public issue for security reports.**

Use GitHub's private vulnerability reporting — *Security → Report a vulnerability* on this
repository. If that is unavailable to you, mail **benjamin.letzel@protonmail.com** instead.

Please include:

- affected component (`core` projection, `store`, public read API, ingest API, CLI, SDK)
- a reproduction or proof-of-concept — for a projection bypass, the collection schema plus the
  payload that got through is usually enough
- the impact you observed
- a suggested remediation, if you have one
- the disclosure timeline you would like; the default is 90 days, flexed by severity

You get an acknowledgement within **3 business days** and a triage decision — accept, decline,
or needs-info — within **10 business days**, then updates until the issue is closed.

## What counts as a vulnerability here

hydrant serves an unauthenticated public read API on purpose, so the usual "no auth = finding"
shortcut does not apply. The threat model is narrower and sharper: **nothing may reach a
response that the collection schema did not release.** In scope:

- **Projection bypass.** Any field reaching a response, a manifest, a digest or the change feed
  that the collection's allow-list does not name. This is the highest-severity class in the
  project — a field that was never supposed to be stored is a data leak, not a bug.
- **Deny-by-default escape.** A schema construct that behaves as a wildcard, an unknown key
  surviving ingest, or a nested object whose members are not individually allow-listed.
- **Ingest authentication.** Token forgery, a non-constant-time comparison, a token recoverable
  from storage or logs, or a source label accepted from an unauthenticated caller.
- **Canonicalization defects.** Two distinct payloads producing the same `content_hash`, or the
  same payload hashing differently across implementations — either one breaks idempotency and
  drift detection.
- **Raw payload retention.** Any read path that reaches retained unprojected payloads when the
  feature is enabled, or retention happening while it is configured off.
- **Availability of the public surface.** A single unauthenticated request that bypasses the
  page-size cap, the statement timeout or the response-size cap, or that reaches an unindexed
  scan through a declared filter.

## What does not count

These are documented design decisions, not findings. Reporting them is fine, but they will be
declined with a pointer here:

- **The read API has no authentication and no per-consumer authorization.** If a record needs
  an audience check, it does not belong in this store.
- **Records an operator released deliberately.** hydrant enforces the allow-list; it cannot
  know that a released field was a bad idea. A wrong schema is an operator bug.
- **`source` provides no isolation.** It is a partitioning label. Everything in the store is
  public, and one source's data being readable by anyone is the intended behaviour.
- **Absence of writes, joins, aggregation or full-text search.** Non-goals, not gaps.
- **Vulnerabilities in a source system or its sender adapter.** Report those to their owners;
  translation lives at the sender by design.

## Coordinated disclosure

If a CVE is assigned we coordinate the publication date with you. Default: a GitHub Security
Advisory plus a `CHANGELOG.md` entry on the day the patched version ships. Say in your report
whether you want attribution in the advisory or prefer to stay anonymous — either is fine.
