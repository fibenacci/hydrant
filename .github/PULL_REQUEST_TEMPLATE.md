## What this changes

<!-- One paragraph. What the change does and why now. Link the issue if there is one. -->

## Type

<!-- Keep the PR to one type; split otherwise. Matches the commit convention in CONTRIBUTING.md. -->

- [ ] `feat` — new feature
- [ ] `fix` — bug fix
- [ ] `refactor` / `perf`
- [ ] `docs`
- [ ] `test`
- [ ] `ci` / `build` / `chore`

## Invariant check

Tick what applies, and say why if a box stays unticked.

- [ ] The service still knows nothing about the source domain — no per-entity handling, no
      foreign-key resolution, no ordering between collection types.
- [ ] Projection still happens at ingest only. No filtering was added on the read path.
- [ ] Deny-by-default holds: unknown keys are dropped and counted, no construct behaves as a
      wildcard.
- [ ] The canonical form (RFC 8785 + SHA-256) is unchanged.
- [ ] Ingest stays idempotent over `content_hash` — an identical payload advances no `seq`.

## Schema / public surface

- [ ] No collection schema changed.
- [ ] A schema changed. Fields added to an allow-list are listed here, with what they contain
      and why they may be released:

<!-- field → content → why public -->

- [ ] The public HTTP surface changed, and the change is documented in the same PR.

## Verification

<!-- What you ran. `make ci` output, or the scoped commands plus the reason the full run can wait. -->

```
```
