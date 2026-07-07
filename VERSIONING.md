# Versioning

`mtp2` follows [Semantic Versioning 2.0.0](https://semver.org/).

Unlike an application, **mtp2 is a library - its public Rust API _is_ the
contract.** Everything reachable as `pub` from the crate root (the framing types
`SignalUnit` / `SuHeader` / `StatusIndication`, the `Mtp2Link` state machine and
`Mtp2State` / `Event` / `Mtp2Config`, the `RetransmissionMethod`, the `monitor`
types, and `Mtp2Error`) is covered. The Python surface tracks the same contract.

## The git tag is the source of truth

`Cargo.toml`'s `version` is set to match the release tag, and the release
workflow's `verify-version` job **refuses to publish** if they disagree. To
release, bump `version`, commit, tag `vX.Y.Z`, and push the tag - the tag push
publishes the crate (and, when enabled, the wheels) at `X.Y.Z`.

## The rule

**MAJOR (`X.0.0`)** - breaks the public API:

- Remove / rename / change the signature of a `pub` item.
- Change documented behavior in a way that breaks existing callers.
- Removals happen only **one minor after** a deprecation.

**MINOR (`x.Y.0`)** - backward-compatible additions:

- New `pub` items (SU shapes, state-machine hooks, config fields, constants).
  This is where the future **E1/T1 card-driver binding** lands - behind its own
  feature flag or sibling crate, additive to this pure core.
- **Deprecations** - mark deprecated, keep it working (removal is the next major).
- An MSRV bump (called out in the changelog).

**PATCH (`x.y.Z`)** - backward-compatible fixes:

- Bug fixes, performance improvements, behavior-neutral dependency bumps.
- **Q.703 conformance corrections** - *even when they change observable wire
  behavior.* The contract is "spec-compliant", so a correction toward the
  specification is a fix, not a break. **Document it loudly in the changelog.**

## Pre-releases

`X.Y.Z-rc.N` for validation before a stable tag. The crates.io "newest" pointer
advances only on stable releases.
