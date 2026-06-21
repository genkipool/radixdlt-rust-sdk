# Contributing

Thanks for your interest in the RadixDLT Rust SDK!

## Repository layout

This repo contains **three** Cargo workspaces (separate because their dependency
trees cannot be resolved together):

- the **main workspace** (`Cargo.toml` at the root): `i18n`, `address`, `rola`,
  `keystore`, `gateway-tx`, `sdk`;
- `crates/connect` — its own workspace (WebRTC dependency tree);
- `crates/connect-iroh` — its own workspace (QUIC/iroh dependency tree).

## Before you open a pull request

Run, for each workspace you touched:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features    # -D warnings in CI
cargo test
```

For the isolated workspaces, run the commands from inside `crates/connect` and
`crates/connect-iroh` respectively. CI runs all three.

## Conventions

- **Code comments are written in English.**
- **User-facing text** (error messages, CLI output) is **bilingual (English/Spanish)**
  via `radixdlt-i18n`: add both variants with the `tr!` macro and keep error types as
  structured enums whose `Display` is localized.
- Public items should be documented (the doc comments feed docs.rs).
- Follow [Semantic Versioning](https://semver.org/); note user-facing changes in
  `CHANGELOG.md`.

## License

By contributing, you agree that your contributions are licensed under MIT OR
Apache-2.0, the same terms as the project.
