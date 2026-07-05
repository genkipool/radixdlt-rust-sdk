# radixdlt-i18n

*[English](README.md) · **Español***

Detección del idioma del sistema y helpers de mensajes bilingües (inglés/español) para
el **RadixDLT Rust SDK**. Cada crate del SDK localiza su texto de error visible al
idioma del sistema usando este crate.

```toml
[dependencies]
radixdlt-i18n = "0.1"
```

```rust
use radixdlt_i18n::{Lang, tr};

let lang = Lang::detect(); // RADIXDLT_LANG | LC_ALL | LC_MESSAGES | LANG → Es/En
let msg = tr!(lang, "invalid key".to_string(), "clave inválida".to_string());
// Forma etiquetada, lista para más idiomas (los no listados caen a inglés):
let msg = tr!(lang, "invalid key".to_string(), Es: "clave inválida".to_string());
```

`Lang` es `#[non_exhaustive]` y `tr!` siempre cae a inglés, así que se pueden añadir
nuevos idiomas sin romper los usos existentes.

Forma parte del [RadixDLT Rust SDK](https://crates.io/crates/radixdlt-sdk).

## Licencia

Publicado bajo MIT o Apache-2.0, a tu elección.
