# Publishing the RadixDLT Rust SDK to crates.io

> ⚠️ **Publishing is irreversible.** A published version can never be deleted, only
> *yanked* (hidden from new resolutions). It stays public and indexed. Read the
> "Decisions before you publish" section first.

## Status

Everything is publish-ready and verified locally:

- All 8 crates have valid metadata (`description`, `license = "MIT OR Apache-2.0"`,
  `repository`, `keywords`, `categories`) and a `README.md` (included in the package).
- `cargo test` is green across the workspace; `radixdlt-i18n` passes a full
  `cargo publish --dry-run`.
- Dependent crates can only be **build-verified** once their dependencies are on
  crates.io — that is why their dry-run reports "no matching package"; it resolves
  itself as you publish in order.

## Decisions before you publish

1. **Name / namespace (`radixdlt-*`).** This is the Radix foundation's brand. On
   crates.io names are first-come-first-served, but Radix could object or request a
   transfer, and the names may look official. Confirm the names are free and that you
   are comfortable owning them, or choose a different prefix.
2. **Repository URL.** The crates declare
   `repository = "https://github.com/genkipool/radixdlt-rust-sdk"`. Create that repo
   (or set the real URL) before publishing.
3. **crates.io account.** You need an account with a **verified email** and an API
   token (https://crates.io/settings/tokens).

## One-time setup

```bash
cargo login <YOUR_CRATES_IO_TOKEN>
```

## Publish order (dependencies first)

Run from `sdk/` for the main-workspace crates:

```bash
cargo publish -p radixdlt-i18n
cargo publish -p radixdlt-address      # needs i18n
cargo publish -p radixdlt-rola         # needs i18n, address
cargo publish -p radixdlt-keystore     # needs i18n, address
cargo publish -p radixdlt-gateway-tx   # needs i18n
cargo publish -p radixdlt-sdk          # needs all of the above
```

The two transport crates are isolated workspaces — publish them from their own dirs
(any time after `radixdlt-i18n`):

```bash
( cd crates/connect      && cargo publish )   # needs i18n
( cd crates/connect-iroh && cargo publish )   # needs i18n
```

Tip: append `--dry-run` to any command to rehearse it (after its dependencies are
already published, the dry-run will also build-verify).

## GitHub repo

```bash
cd sdk
git init && git add . && git commit -m "RadixDLT Rust SDK 0.1.0"
git branch -M main
git remote add origin https://github.com/<you>/radixdlt-rust-sdk.git
git push -u origin main
```

(`crates/connect` and `crates/connect-iroh` are isolated cargo workspaces but live in
the same Git repo — that is fine.)

## Notes

- License files (`LICENSE-MIT`, `LICENSE-APACHE`) live at the repo root. Individual
  crate packages carry the SPDX `license` field, which is what crates.io requires;
  per-crate license files are optional.
- After publishing, switch the application (`rust/crates/*`, `protosign`) from the
  `path = "../../../sdk/crates/*"` dependencies to the published versions
  (`radixdlt-rola = "0.1"`, etc.).
