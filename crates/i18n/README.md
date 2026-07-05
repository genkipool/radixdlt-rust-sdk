# radixdlt-i18n

System-locale detection and bilingual (English/Spanish) message helpers for the
**RadixDLT Rust SDK**. Every SDK crate localizes its user-facing error text to the
system language using this crate.

***English** · [Español](README.es.md)*

```toml
[dependencies]
radixdlt-i18n = "0.1"
```

```rust
use radixdlt_i18n::{Lang, tr};

let lang = Lang::detect(); // RADIXDLT_LANG | LC_ALL | LC_MESSAGES | LANG → Es/En
let msg = tr!(lang, "invalid key".to_string(), "clave inválida".to_string());
// Labelled form, ready for more languages (unlisted ones fall back to English):
let msg = tr!(lang, "invalid key".to_string(), Es: "clave inválida".to_string());
```

`Lang` is `#[non_exhaustive]` and `tr!` always falls back to English, so new
languages can be added without breaking existing call sites.

Part of the [RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## License

Licensed under either of MIT or Apache-2.0 at your option.
