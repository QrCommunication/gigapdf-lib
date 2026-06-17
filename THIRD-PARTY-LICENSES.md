# Third-Party Licenses

The GigaPDF engine itself — the PDF/Office/image parser, object model, content
editor, serializer, rasteriser, font subsystem, OCR and all the document-format
conversions — is **pure Rust `std` with no third-party library**, and is licensed
under the PolyForm Noncommercial License 1.0.0 (see [`LICENSE`](./LICENSE)).

Two subsystems are the deliberate, documented exceptions to the no-dependency
rule, because doing them in-house is a liability rather than a feature:

1. **Cryptography** — rolling your own RSA/AES/SHA/ASN.1/X.509/CMS invites timing
   and padding-oracle side-channels and carries no audit pedigree. The engine
   delegates to the [RustCrypto](https://github.com/RustCrypto) crates, whose
   constant-time primitives and reviewed ASN.1/X.509/CMS are used instead.
2. **JavaScript** — a hand-written JS engine is a multi-year specification
   maintenance burden. The HTML→PDF inline-`<script>` path uses
   [Boa](https://github.com/boa-dev/boa) (`boa_engine`), a pure-Rust engine.

These dependencies are all permissively licensed and compatible with a
noncommercial product. Where a crate is dual-licensed, GigaPDF elects the **MIT**
terms (reproduced below).

## Dependencies and licenses

| Crate | Upstream license | Elected | Role |
|-------|------------------|---------|------|
| `rsa` | MIT OR Apache-2.0 | MIT | RSA keygen + PKCS#1 v1.5 signing (blinded) |
| `sha2`, `sha1`, `md-5` | MIT OR Apache-2.0 | MIT | SHA-2 / SHA-1 / MD5 hashing |
| `hmac` | MIT OR Apache-2.0 | MIT | HMAC (PBKDF2, PKCS#12 MAC) |
| `aes`, `cbc` | MIT OR Apache-2.0 | MIT | AES-CBC (PDF security handler, PBES2) |
| `des`, `rc2` | MIT OR Apache-2.0 | MIT | Legacy PKCS#12 PBES1 ciphers |
| `rand`, `getrandom` | MIT OR Apache-2.0 | MIT | CSPRNG / platform entropy (signing) |
| `der`, `const-oid`, `spki` | Apache-2.0 OR MIT | MIT | ASN.1 DER, OIDs, SubjectPublicKeyInfo |
| `signature` | Apache-2.0 OR MIT | MIT | Digital-signature traits |
| `x509-cert` | Apache-2.0 OR MIT | MIT | X.509 certificate build/parse |
| `cms` | Apache-2.0 OR MIT | MIT | CMS / PKCS#7 SignedData |
| `boa_engine` | Unlicense OR MIT | MIT | Embedded JavaScript engine |

(Each pulls its own transitive dependencies, which are likewise MIT/Apache-2.0 or
more permissive; run `cargo deny` / `cargo about` for the full transitive list.)

## MIT License

The RustCrypto crates are © The RustCrypto Project Developers. `boa_engine` is
© The Boa Developers. `rand` / `getrandom` are © The Rand Project Developers.
Each is provided under the MIT License:

```
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

`boa_engine` may alternatively be used under the [Unlicense](https://unlicense.org/)
(public domain); GigaPDF elects MIT for a single, consistent attribution regime.
