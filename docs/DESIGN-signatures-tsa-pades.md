# DESIGN — Trusted Timestamps (RFC 3161), PAdES & LTV signatures

> Status: **P1 + P2 + P3 IMPLEMENTED** (B-T, PAdES-B default, B-LT/B-LTA LTV).
> P1/P2 shipped in 0.70.0; **P3 (LTV) landed 2026-06-23** (this pass) — see
> §C "Implemented" and the phase table.
> Scope: `gigapdf-lib` (Rust → WASM, pure-`std` core + audited RustCrypto for crypto).
> Author: design pass, 2026-06-23. Repo: `QrCommunication/gigapdf-lib` @ workspace `0.70.0`.

---

## 0. Why this epic

The signature stack today produces a **valid but short-term** PDF signature:
a detached CMS/PKCS#7 `SignedData` under the `adbe.pkcs7.detached` subfilter,
either self-signed (ephemeral digital ID) or from an imported PKCS#12 identity.

In PAdES terms that is a **B-B level** signature (signature + signing
certificate, no trusted time, no revocation material). The remaining
high-value gap for eIDAS / European e-invoicing / archival use cases is:

| Need | PAdES level | What it adds |
|------|-------------|--------------|
| Prove the signature existed at a trusted time | **B-T** | An RFC 3161 timestamp token from a TSA, embedded in the SignerInfo |
| Self-contained long-term validation | **B-LT** | Cert chain + OCSP/CRL revocation material in a DSS |
| Long-term archival, renewable | **B-LTA** | A document-level timestamp over the whole DSS |

This is the last forte-value gap in the engine and the only one that pulls in a
genuinely new architectural problem: **the timestamp requires a network round
trip, and the core has no network stack.**

---

## 1. Current state (what exists, file:line)

### 1.1 The CMS is already attribute-bearing and synchronous

`crates/core/src/sign/mod.rs`

- `Signer::generate()` (`mod.rs:79`) builds a self-signed X.509 cert via
  `x509-cert`'s `CertificateBuilder` (`mod.rs:97`).
- `build_detached_cms()` (`mod.rs:125`) builds the `SignedData`:
  - `EncapsulatedContentInfo { econtent_type: id-data, econtent: None }` —
    **detached** (`mod.rs:129`).
  - `SignerInfoBuilder::new(&signing_key, sid, sha256_alg(), &econtent, Some(&digest))`
    (`mod.rs:138`). **Passing `Some(&digest)` makes the builder emit *signed
    attributes*** — `messageDigest` + `contentType` — and sign over their DER.
    (Confirmed in `cms-0.2.3/src/builder.rs:217-264`: when a message digest is
    supplied, `signed_attributes` are populated and the signature covers them.)
- `detached_cms_external()` (`mod.rs:157`) is the same path for a PKCS#12 key/cert.
- `issuer_and_serial()` (`mod.rs:169`) extracts issuer + serial TLVs.

**The whole signing path is synchronous and performs zero I/O / zero network.**
It runs entirely inside `ctx.eval`-free Rust; the only host dependency is
entropy (RSA blinding / keygen).

### 1.2 Embedding into the PDF — `/ByteRange` + `/Contents` patching

`crates/core/src/document.rs`

- `Document::sign()` (`document.rs:1049`) and `sign_p12()` (`document.rs:1069`)
  funnel through `sign_with()` (`document.rs:1096`).
- `sign_with()`:
  - Writes a `/Sig` value dict with **`SubFilter = adbe.pkcs7.detached`
    hardcoded** (`document.rs:1114`).
  - Reserves `CONTENTS_BYTES = 8192` (→ 16384 hex chars) for the CMS
    (`document.rs:1105`).
  - Adds an invisible signature widget + AcroForm field, `SigFlags = 3`.
  - Serializes, patches `/ByteRange` over the 4 placeholder integers
    (`document.rs:1214`), computes `signed` = everything except the `/Contents`
    hex window (`document.rs:1224`), calls `build_cms(&signed)`, hex-fills
    `/Contents` (`document.rs:1242`).
  - Hard cap: errors if the CMS exceeds the reserved space (`document.rs:1234`).

> **Design consequence:** the embedding machinery is already correct and
> subfilter-agnostic in structure — only the literal `adbe.pkcs7.detached` name
> and the `8192`-byte reservation are tied to the legacy profile. PAdES needs a
> different subfilter name and a **larger** `/Contents` reservation (a TST token
> + chain + the unsigned-attr wrapper is materially bigger).

### 1.3 The DER toolkit already in-tree

`crates/core/src/sign/der.rs` is a **complete definite-length DER
builder + reader**, already used by the PKCS#12 importer:

- Builders: `tlv`, `sequence` (`der.rs:108`), `set` (`der.rs:113`), `oid`
  (`der.rs:86`), `integer`/`integer_u32`, `octet_string` (`der.rs:96`),
  `bit_string`, `utc_time`, `context`/`context_primitive`.
- Reader: `Reader::new`, `read`, `next_tag`, `descend` (`der.rs:242`),
  `Tlv::is_oid`.

This matters: **TimeStampReq construction and TSTInfo parsing can be done with
these primitives — no new ASN.1 crate is required** (see §A.5).

### 1.4 The crypto deps (verified `crates/core/Cargo.toml`)

```
rsa 0.9 (+sha2)   sha2 0.10 (+oid)   sha1 0.10   md-5 0.10   hmac 0.12
aes 0.8  cbc 0.1  des 0.8  rc2 0.8
der 0.7  const-oid 0.9  spki 0.7  x509-cert 0.2 (+builder)  cms 0.2 (+builder)
```

What `cms 0.2.3` gives us for this epic (verified in the vendored source):

- `SignerInfoBuilder::add_unsigned_attribute(Attribute)` —
  `cms-0.2.3/src/builder.rs:148`. **This is exactly the hook for the RFC 3161
  timestamp token** (an unsigned attribute on the SignerInfo).
- `SignedDataBuilder::add_crl(RevocationInfoChoice)` — `builder.rs:368`. Useful
  for LTV (CRLs can also live in `SignedData.crls`, though PAdES prefers the DSS).
- `cms::content_info::ContentInfo` (`content_info.rs`) — the TSA's response token
  is itself a CMS `ContentInfo`; we wrap its DER as the attribute value.

What is **missing** from the dependency set for this epic:

- **No `tsp` crate.** RustCrypto's `tsp` crate (which defines `TimeStampReq`,
  `TimeStampResp`, `MessageImprint`, `TstInfo`, `TspStatus`) is **not** a
  dependency and is **not vendored** (confirmed: no `tsp-*` in the local
  registry). So the RFC 3161 ASN.1 must come from one of:
  - **(M1) Hand-roll with `sign/der.rs`** — TimeStampReq is tiny (≈6 fields);
    TSTInfo parsing only needs to read `genTime`, the `messageImprint` and the
    hashed-message OCTET STRING. Keeps the zero-extra-dep posture. **Recommended.**
  - **(M2) Add the `tsp` crate** (RustCrypto, MIT/Apache, wasm-clean). Smaller
    code, but adds a 4th formats crate to the "narrow crypto exception" list.
- **No OCSP / CRL fetch or parse** for LTV. RustCrypto has an `x509-ocsp` crate
  (request building + response parsing); CRLs are parseable via `x509-cert`'s
  `CertificateList`. Both are **not yet** dependencies (LTV is P3, decide later).

### 1.5 The SDK surface

`sdk/src/index.ts`

- `GigaPdfDoc.sign(fields, random, keyBits=2048)` (`index.ts:2859`) →
  `gp_sign` (`crates/wasm/src/lib.rs:237`).
- `GigaPdfDoc.signP12(p12, password, opts)` (`index.ts:2876`) →
  `gp_sign_p12` (`crates/wasm/src/lib.rs:282`); `SignP12Options` at `index.ts:1628`.
- Both are **synchronous** TS methods returning `Uint8Array`.

### 1.6 The host-fetch precedent — the crux of the design

The engine **never performs HTTP**; the WASM sandbox has no network stack
(invariant restated at `crates/core/src/font/google.rs:1-15`). Two host-boundary
patterns already exist and bound the entire design space for TSA:

**Pattern A — synchronous host import (entropy).**
`crates/wasm/src/rng.rs:21` declares `extern "C" { fn gp_host_random(dest, len); }`;
the SDK supplies `env.gp_host_random` from `crypto.getRandomValues`
(`sdk/src/index.ts:96` notes the wiring). This works **only because
`getRandomValues` is synchronous in JS.** `fetch` is **not** — so a TSA POST
**cannot** reuse this pattern without synchronous-XHR or `Atomics.wait`
gymnastics (see §A.3, rejected).

**Pattern B — two-phase pure-data (HTML/font resources).**
The core computes *what to fetch* and parses *what the host fetched*; the host
runs the actual network loop in between:
- `html::needed_resources(html, header, footer)` →
  `gp_html_needed_resources` (`crates/wasm/src/lib.rs:2834`) returns the URL list.
- Host fetches each URL (browser/Node `fetch`), with anti-SSRF host pinning
  (`google::is_gstatic_url`, `font/google.rs:26`).
- `html::render_with(html, fonts, opts)` / `gp_html_render_with`
  (`crates/wasm/src/lib.rs:2795`) consumes the fetched bytes.

**This is the model the TSA flow must follow.** (§A.3 makes it concrete.)

---

## 2. The central architectural challenge

> **An RFC 3161 timestamp is, by definition, a network operation (POST the
> signature hash to a TSA, receive a signed token). The core is pure-`std` with
> no network stack and must stay that way. A PDF signature is also a single
> atomic `/ByteRange`+`/Contents` patch: the timestamp token must be present
> *before* `/Contents` is finalized, because the token becomes part of the bytes
> the reader validates.**

So we cannot "sign, return the PDF, then go fetch a timestamp and bolt it on" —
not for the *signature* timestamp (B-T), which lives **inside** the SignerInfo
that is hashed into `/Contents`. (A *document* timestamp for B-LTA is different:
it is an incremental-update second signature and *can* be a separate pass.)

The signature path must therefore become **interruptible**: build everything up
to the point where we know the bytes to be timestamped, **suspend, let the host
do the TSA HTTP, resume** with the token, finalize `/Contents`. Three options:

### Option 1 — Sync host import for the TSA POST (Pattern A) — **REJECTED**

Declare `extern "C" { fn gp_host_tsa(req_ptr, req_len, out_ptr, out_cap) -> usize; }`
and have the SDK implement it synchronously. Rejected because:
- JS `fetch` is async; a *synchronous* implementation needs either deprecated
  synchronous `XMLHttpRequest` (unavailable in Node, banned on the main thread
  in modern browsers) or `SharedArrayBuffer` + `Atomics.wait` in a Worker
  (requires cross-origin isolation headers, a worker harness, and still blocks).
- It buries a blocking network call inside the WASM call boundary — the opposite
  of the engine's "core computes, host does I/O" invariant.

### Option 2 — Two-phase: core emits a TimeStampReq, host POSTs, core resumes (Pattern B) — **RECOMMENDED**

Split signing into two ABI calls around the host's TSA round trip:

```
Phase 1  (core, sync)   doc.sign_prepare_timestamped(field opts, signer)
                        → builds the SignerInfo *signature* (signs the signed
                          attrs), computes the DER it will timestamp
                          (= the SignerInfo.signature OCTET STRING per RFC 3161
                          §A appendix / PAdES), builds the RFC 3161 TimeStampReq
                          DER, and returns BOTH:
                            • the TimeStampReq bytes (host POSTs these), and
                            • an opaque "pending signature" handle holding all
                              partial state (cert, signed attrs, signature,
                              field dict, reserved byte budget).

Host    (JS, async)     POST TimeStampReq to the TSA URL with
                        Content-Type: application/timestamp-query,
                        read application/timestamp-reply, extract the
                        TimeStampToken (a CMS ContentInfo) from TimeStampResp.

Phase 2  (core, sync)   doc.sign_finish_timestamped(pending, tst_token_der)
                        → validates the token shape, adds it as the
                          id-aa-timeStampToken UNSIGNED attribute on the
                          SignerInfo, rebuilds SignedData, patches /Contents.
                        → returns the signed PDF bytes (B-T).
```

This mirrors `needed_resources` → host fetch → `render_with` **exactly**. The
core stays synchronous and network-free; the SDK exposes **one `async` method**
that orchestrates the round trip. **This is the recommended design.**

Trade-off: introduces a short-lived "pending signature" state object across two
WASM calls (like the `Document` handle already crossing the boundary). The state
must hold the *exact* bytes/struct that Phase 2 finalizes, so the two phases are
internally consistent.

### Option 3 — Keep signing sync; do the timestamp as a separate document-timestamp pass — **partial only**

A *document timestamp* (a signature of subfilter `ETSI.RFC3161` whose `/Contents`
is itself a bare TST, no CMS signer) is a legitimate PAdES construct (it is what
B-LTA adds on top of B-LT). It can be applied as an **incremental update** after
the main signature, so it needs no interruption of the main signing path — it is
its own two-phase op (build req over the doc's ByteRange digest → host POST →
embed token).

But a document timestamp is **not** a substitute for a *signature* timestamp
(B-T): B-T requires the TST inside the *signer's* unsigned attributes. So Option
3 covers B-LTA's doc-timestamp and a "timestamp-only" feature, but B-T still
needs Option 2. **We adopt Option 2 for B-T and reuse the same two-phase plumbing
for the Option-3 document timestamp in P3.**

**Recommendation: Option 2**, with the two-phase plumbing reused for B-LTA's
document timestamp.

---

## A. TSA timestamp (RFC 3161) — detailed design

### A.1 The RFC 3161 round trip

```
       signature value (OCTET STRING)            TSA
              │  SHA-256                           │
              ▼                                     │
        MessageImprint ──► TimeStampReq (DER) ──────►  (POST application/timestamp-query)
                                                     │
        TimeStampResp  ◄───────────────────────────  (application/timestamp-reply)
              │
              ├─ PKIStatusInfo (must be granted/0 or grantedWithMods/1)
              └─ timeStampToken : ContentInfo(SignedData) whose eContent is TSTInfo
                       │
                       ▼
        embed token as id-aa-timeStampToken UNSIGNED attr on the SignerInfo
```

What gets hashed for the `MessageImprint`: per RFC 3161 / PAdES, the timestamp
is computed over the **signature value** of the SignerInfo being timestamped
(the `signature` OCTET STRING), **not** over the document. This is why Phase 1
must complete the SignerInfo signature before it can build the request.

### A.2 The ASN.1 we must produce/consume

**TimeStampReq (we build — RFC 3161 §2.4.1):**
```
TimeStampReq ::= SEQUENCE {
  version        INTEGER { v1(1) },
  messageImprint MessageImprint,          -- AlgorithmIdentifier + hashedMessage
  reqPolicy      TSAPolicyId      OPTIONAL,
  nonce          INTEGER          OPTIONAL,   -- include: 64–128 random bits
  certReq        BOOLEAN DEFAULT FALSE }      -- set TRUE: we want the TSA cert in the token

MessageImprint ::= SEQUENCE {
  hashAlgorithm  AlgorithmIdentifier,         -- id-sha256
  hashedMessage  OCTET STRING }               -- SHA-256(signature value)
```

**TimeStampResp / token (we parse — RFC 3161 §2.4.2):**
```
TimeStampResp ::= SEQUENCE {
  status         PKIStatusInfo,               -- status INTEGER: 0/1 = OK
  timeStampToken TimeStampToken OPTIONAL }    -- a CMS ContentInfo (SignedData)

TSTInfo ::= SEQUENCE {                         -- the SignedData eContent
  version, policy, messageImprint, serialNumber,
  genTime GeneralizedTime, ... }
```

We do **not** need to re-encode TSTInfo; we embed the **token (the
`TimeStampToken` ContentInfo) verbatim** as the unsigned-attribute value.

### A.3 Where the bytes cross the host boundary

New WASM ABI (mirrors `gp_html_needed_resources` / `gp_html_render_with`):

```rust
// Phase 1: returns the DER TimeStampReq to POST, and stashes pending state
//          keyed to `handle`. Buffer-returning (host frees); null on error.
pub extern "C" fn gp_sign_prepare_tsa(
    handle: *mut Document,
    fields_ptr, fields_len,         // same tab-joined field metadata
    rand_ptr, rand_len, bits,       // self-signed variant; OR a p12 variant
    out_len) -> *mut u8;            // → TimeStampReq DER

// Phase 2: consumes the TSA token, finalizes the signed PDF. null on error.
pub extern "C" fn gp_sign_finish_tsa(
    handle: *mut Document,
    token_ptr, token_len,           // raw TimeStampToken (ContentInfo) bytes
    out_len) -> *mut u8;            // → signed PDF (B-T)
```

The "pending state" lives in the core keyed by the document handle (a
`Option<PendingTimestampSignature>` field on `Document`, cleared on finish/drop)
— the same lifetime model the `Document` handle already uses.

SDK (`sdk/src/index.ts`) gains **one async method**:

```ts
interface SignTsaOptions extends SignP12Options {
  tsaUrl: string;                    // e.g. "https://freetsa.org/tsr"
  tsaFetch?: (req: Uint8Array, url: string) => Promise<Uint8Array>; // override
}

async signTimestamped(opts: SignTsaOptions): Promise<Uint8Array> {
  const req = /* gp_sign_prepare_tsa(...) */;
  const token = opts.tsaFetch
    ? await opts.tsaFetch(req, opts.tsaUrl)
    : await defaultTsaPost(opts.tsaUrl, req);   // fetch() POST, sync XHR-free
  return /* gp_sign_finish_tsa(token) */;
}

// Node & browser both: POST application/timestamp-query, read the reply body.
async function defaultTsaPost(url: string, req: Uint8Array): Promise<Uint8Array> {
  const r = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/timestamp-query" },
    body: req,
  });
  if (!r.ok) throw new Error(`TSA HTTP ${r.status}`);
  return new Uint8Array(await r.arrayBuffer());
}
```

Why a `tsaFetch` injection hook: lets the host add auth headers, proxies,
retries, **and apply its own SSRF allow-list** (the core can only host-pin the
*scheme*; the URL is host-controlled, so SSRF policy is a host responsibility,
exactly as with the Google-Fonts host-pin today). The SDK default `fetch` keeps
the common case one-liner.

### A.4 Embedding the token in the CMS

Phase 2 adds the token as an **unsigned attribute** via the existing builder:

```rust
use cms::cert::x509::attr::{Attribute, AttributeValue};
// OID id-aa-timeStampToken = 1.2.840.113549.1.9.16.2.14
let oid = const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.2.14");
let token_any = ::der::Any::from_der(token_der)?;          // the ContentInfo
let attr = Attribute { oid, values: SetOfVec::from_iter([token_any])? };
signer_info_builder.add_unsigned_attribute(attr)?;          // builder.rs:148
```

Because the timestamp is an **unsigned** attribute, **it does not change the
signed-attrs digest** — i.e. Phase 2 does *not* re-sign; it only re-encodes the
`SignedData` with the enlarged SignerInfo. This is the clean reason the two-phase
split is sound: the signature computed in Phase 1 stays valid in Phase 2.

### A.5 What RustCrypto is missing & the fill (decision M1 vs M2)

| Element | In `cms`/`der`/`x509-cert`? | Plan |
|---------|----------------------------|------|
| `id-aa-timeStampToken` unsigned attr | ✅ `add_unsigned_attribute` | use as-is |
| Wrap token `ContentInfo` as `Any` | ✅ `der::Any` | use as-is |
| `TimeStampReq` / `MessageImprint` encode | ❌ (no `tsp`) | **M1: hand-roll with `sign/der.rs`** (≈40 LOC) — or **M2: add `tsp` crate** |
| `TimeStampResp` status + token extract | ❌ | M1: read `status` INTEGER + slice the token TLV via `der::Reader` — or M2: `tsp` |
| TSTInfo `genTime` read (optional surfacing) | ❌ | M1: `Reader::descend` into the token's eContent |

**Recommendation: M1 (hand-roll).** TimeStampReq is trivial and the existing
`der` module is purpose-built for exactly this; it avoids adding a 4th formats
crate and keeps the dependency story ("two narrow crypto exceptions") intact.
Fall back to **M2 (`tsp`)** only if we later need full TSTInfo validation
(verifying the TSA's own signature on the token — see §LTV/verification note).

> Note on **verifying** the token: P1 *embeds* the TSA token trusting the
> transport. Cryptographically *verifying* the TSA's signature on TSTInfo (and
> chaining the TSA cert) is a B-LT/B-LTA concern; it belongs with revocation
> material in the DSS (P3), and is where `tsp` + `x509-ocsp` would earn their keep.

---

## B. PAdES baseline — design

PAdES (ETSI EN 319 142-1) differs from the legacy `adbe.pkcs7.detached` profile
in three concrete, low-risk ways for our embedding code:

1. **SubFilter** → `ETSI.CAdES.detached` (instead of `adbe.pkcs7.detached`).
   One literal change in `sign_with()` (`document.rs:1114`), parameterized.
2. **Signed attributes required by the baseline:**
   - `signing-certificate-v2` (ESS, OID `1.2.840.113549.1.9.16.2.47`): a
     `SigningCertificateV2` containing `ESSCertIDv2` = **SHA-256 hash of the
     signer certificate** (+ optional issuerSerial). This binds the signature to
     a specific certificate and is **mandatory** for PAdES-B. Added as a
     *signed* attribute via `SignerInfoBuilder::add_signed_attribute`
     (`builder.rs:138`) **before** the signature is computed.
   - `signing-time` (OID `1.2.840.113549.1.9.16.2.x` → actually
     `1.2.840.113549.1.9.5`) — the claimed signing time. (PAdES allows it; the
     trusted time comes from the B-T timestamp.)
   - `messageDigest` + `contentType` — already emitted (§1.1).
3. **`/Contents` size** — must grow to hold cert chain + (for B-T) the TST token.
   Bump the reservation (`document.rs:1105`) from 8192 to a PAdES-sane size
   (e.g. 16384–30000 bytes) **or** compute it from the actual CMS length with
   headroom (preferred: build CMS once, size `/Contents` to `len + slack`).

ASN.1 to add for `signing-certificate-v2` (hand-rollable with `sign/der.rs`, or
`x509-cert`'s ESS types if available in 0.2):
```
SigningCertificateV2 ::= SEQUENCE { certs SEQUENCE OF ESSCertIDv2 }
ESSCertIDv2 ::= SEQUENCE {
  hashAlgorithm AlgorithmIdentifier DEFAULT id-sha256,
  certHash      OCTET STRING,             -- SHA-256(DER(signerCert))
  issuerSerial  IssuerSerial OPTIONAL }
```

> Design decision: implement PAdES-B as **the new default profile for the
> existing `sign`/`signP12`** (subfilter + signing-certificate-v2), gated behind
> a profile flag so the legacy `adbe.pkcs7.detached` output remains available for
> compatibility. B-T then = PAdES-B + the §A timestamp.

---

## C. LTV — Document Security Store (DSS) + VRI — design

PAdES-B-LT/B-LTA make the signature self-validating long after certs expire by
embedding the **validation material** in a `/DSS` catalog entry:

```
Catalog /DSS  →  << /Certs  [streams of DER certs in the chain]
                    /OCSPs  [streams of DER OCSP responses]
                    /CRLs   [streams of DER CRLs]
                    /VRI    << /<hexUpperSHA1ofSignatureContents> << /Cert.../OCSP.../CRL... >> >> >>
```

- **B-LT** = add `/DSS` with the full chain (user cert → root) + fresh OCSP
  responses **or** CRLs covering each cert. ETSI EN 319 142-1 now *advises
  against VRI* (all material is referenceable from `/DSS` directly), so a modern
  implementation can populate `/DSS/Certs|/OCSPs|/CRLs` and **skip `/VRI`**.
- **B-LTA** = B-LT **plus** a **document timestamp** (subfilter `ETSI.RFC3161`,
  an incremental-update signature whose `/Contents` is a bare TST over the whole
  file's ByteRange) that protects the DSS itself and is renewable.

What the engine must gain for LTV:
- A `/DSS` writer in `document.rs` that adds the dictionary + the cert/OCSP/CRL
  streams as an **incremental update** (so existing signatures stay byte-intact).
  The engine already does append-only object insertion; this is additive.
- The **OCSP/CRL material itself is fetched by the host** (same two-phase model):
  the core can compute *what to fetch* (the AIA OCSP URL / CRL DP from each cert),
  the host fetches, the core embeds. New deps to *parse* responses: `x509-ocsp`
  (OCSP) and `x509-cert::crl` (CRL) — **add only when P3 lands.**
- The B-LTA document timestamp reuses the §A two-phase TSA plumbing.

This is the largest chunk and is explicitly **P3** (gated on real demand for full
LTA archival vs. "B-T is enough for our e-invoicing/eIDAS-advanced use case").

### C.1 Implemented (2026-06-23) — what landed vs. what was deferred

**Built (B-LT + B-LTA), M1 hand-roll, zero new deps:**

- **Incremental-update writer** — `serialize::append_incremental_update(base,
  new_objects, prev_startxref, size, root, info)` keeps `base` byte-for-byte and
  appends a fresh body + xref (per-run subsections) + `/Prev`-chained trailer.
  `serialize::last_startxref` / `last_size` read the chain point. This is the
  mechanism that lets a `/DSS` and a document timestamp be added **without
  invalidating the prior signature's `/ByteRange`** (verified by a test asserting
  the signed bytes are an untouched prefix).
- **`sign/ltv.rs`** (hand-rolled with `sign/der.rs`, no `x509-ocsp`/`tsp`):
  - `certificate_extensions` + `ocsp_url` (AIA `id-ad-ocsp` → `[6]` URI) +
    `crl_url` (CRL-DP, first HTTP(S) URI).
  - `build_ocsp_request` (RFC 6960 `OCSPRequest`, SHA-1 `CertID` over issuer
    DN/SPKI key + subject serial, optional nonce extension).
  - `parse_ocsp_response` (`responseStatus` gate, embedded verbatim) and
    `parse_crl` (`CertificateList` shape check, embedded verbatim).
  - `certificates_from_cms` (pull the chain out of a signature's `/Contents`),
    `plan_chain` (per-cert OCSP-first + CRL-fallback plan, cert *i* checked vs its
    issuer *i+1*), `vri_key` (upper-hex SHA-1 of `/Contents`).
- **`Document` API**: `ltv_fetch_plan` (phase 1 — emit fetch targets) ·
  `apply_dss` (phase 2 — `/DSS` with `/Certs`,`/OCSPs`,`/CRLs`,`/VRI` as an
  incremental update; malformed OCSP/CRL skipped) · `prepare_doc_timestamp` /
  `finish_doc_timestamp` (B-LTA document timestamp, `ETSI.RFC3161`, two-phase,
  `/Contents` = bare token extracted via `timestamp::parse_response`).
- **WASM ABI**: `gp_ltv_targets` (JSON, hex-encoded binary) · `gp_apply_dss`
  (length-framed cert/OCSP/CRL buffers) · `gp_doc_timestamp_prepare` /
  `gp_doc_timestamp_finish`.
- **SDK**: `signLtv(opts)` async — B-T → fetch OCSP/CRL per cert (best-effort;
  unreachable responders skipped) → `/DSS` (B-LT) → optional document timestamp
  (`archiveTimestamp` → B-LTA). `defaultOcspPost` / `defaultCrlGet` (host fetch,
  overridable via `revocationFetch`/`crlFetch` for auth/SSRF allow-list).
- **SSRF**: per design §A.3/§F, the OCSP/CRL URLs come from the **certificates'**
  AIA/CRL-DP extensions and are **host-fetched**; the engine only computes which
  URLs. The SDK defaults add `redirect: "error"` but **no allow-list** — a
  consumer worried about a hostile cert (AIA → internal host) passes
  `revocationFetch`/`crlFetch`/`tsaFetch` to enforce its own policy. Documented on
  the option types.

**Deferred (not needed for B-LT/B-LTA production):**

- **Cryptographic *verification*** of the embedded OCSP/CRL/TSA tokens (signature
  + chain validation) — this is a *consumer/validator* concern, a separate epic,
  and is where `x509-ocsp` + `tsp` would earn their keep. The producer embeds the
  material the host fetched (transport-trusted), exactly as P1 embeds the TSA
  token.
- **CRL freshness / `thisUpdate`/`nextUpdate` time checks** — the engine has no
  clock; the embedded CRL's currency is a host/validator responsibility.

---

## D. TSA providers — survey & default choice

| Provider | URL | Account? | Notes |
|----------|-----|----------|-------|
| **FreeTSA.org** | `https://freetsa.org/tsr` | **No** | Community TSA, RFC 3161, HTTP/HTTPS + Tor. Free, no key/account. Lower SLA — fine for self-signed/dev and many production needs. Its CA cert is published for verification. |
| **DigiCert** | `http://timestamp.digicert.com` | **No** | Public, no account. Widely trusted root; very high reliability. Intended for code/doc signing timestamps. RSA-SHA256. |
| **Sectigo** | `http://timestamp.sectigo.com` | **No** | Public, no account; auto-selects RSA-SHA256/384/512. Reliable. |
| **rfc3161.ai.moda** | `https://rfc3161.ai.moda` | No | Community, serves millions/month. |
| **sigstore** | self-hostable (`sigstore/timestamp-authority`) | self-host | Run your own TSA if a trusted in-house root is wanted. |
| Qualified TSU (eIDAS QTSP) | per-provider | **Yes (paid)** | Required only for **qualified** timestamps (eIDAS Art. 42 "qualified electronic time stamp"). Out of scope unless we target QES/qualified. |

**Default recommendation:** make the TSA **fully URL-configurable** (no provider
hardcoded). Ship documentation defaulting to **FreeTSA.org** for the
zero-friction/self-signed path and **DigiCert** as the "trusted-root, no-account"
production default. **Do not bake a provider into the engine** — the core only
emits the request; the URL is a host/SDK argument.

> eIDAS reality check: FreeTSA/DigiCert/Sectigo give **trusted (RFC 3161)**
> timestamps, sufficient for **PAdES-B-T / "advanced"** signatures. A
> **qualified** electronic timestamp (for QES-grade eIDAS) requires a
> **Qualified TSP on the EU Trust List** — a paid, configured provider. That is a
> deployment/config choice, not an engine capability; the engine works with any
> RFC 3161 TSA URL the host provides.

---

## E. Phases, effort, decisions

### Phase plan

| Phase | Deliverable | Core work | Effort |
|-------|-------------|-----------|--------|
| **P1 — TSA timestamp (B-T)** | `signTimestamped()` async SDK method; two-phase `gp_sign_prepare_tsa`/`gp_sign_finish_tsa`; TimeStampReq build (M1) + token embed as unsigned attr; works on top of self-signed **and** PKCS#12; PAdES-B signed attrs (signing-certificate-v2) + `ETSI.CAdES.detached` subfilter so the timestamped output is genuine PAdES-B-T; bigger/auto-sized `/Contents`. Tests: req DER shape, finish with a captured token fixture, end-to-end against FreeTSA behind the `tsaFetch` hook. | `sign/timestamp.rs` (req build/resp parse, M1), `sign/pades.rs` (ESS signing-cert-v2), `Document::sign_prepare_timestamped`/`sign_finish_timestamped` + pending state, 2 wasm ABI fns, SDK async method + `defaultTsaPost`, `/Contents` sizing. | **~M (4–6 dev-days)** |
| **P2 — PAdES-B as default profile** | Profile flag on `sign`/`signP12` selecting `ETSI.CAdES.detached` + ESS attrs vs legacy `adbe.pkcs7.detached`; B-T = profile + P1 timestamp. (Largely folded into P1; this phase is the cleanup + the non-timestamped PAdES-B path + docs.) | Parameterize `sign_with` subfilter + attr set; SDK `SignProfile`. | **~S (1–2 dev-days)** |
| **P3 — PAdES-LTV (B-LT / B-LTA)** ✅ **DONE (2026-06-23)** | `/DSS` writer (incremental update) for cert chain + OCSP/CRL; host two-phase fetch of OCSP/CRL (compute AIA/CRL-DP URLs in core, host fetches); document timestamp (`ETSI.RFC3161`) reusing P1 plumbing for B-LTA. **Decision: M1 hand-roll (no `x509-ocsp`/`tsp` dep)** — OCSP request/response + CRL shape + AIA/CRL-DP extraction done with the in-tree `sign/der.rs`, keeping the "two narrow crypto exceptions" posture (`cms` + `x509-cert`). | `serialize::append_incremental_update`/`last_startxref`/`last_size`; `sign/ltv.rs` (AIA/CRL-DP discovery, OCSP req build, OCSP/CRL parse, CMS chain extract, VRI key); `Document::ltv_fetch_plan`/`apply_dss`/`prepare_doc_timestamp`/`finish_doc_timestamp` + pending state; 4 wasm ABI fns (`gp_ltv_targets`/`gp_apply_dss`/`gp_doc_timestamp_prepare`/`gp_doc_timestamp_finish`); SDK `signLtv()` async + `defaultOcspPost`/`defaultCrlGet`. | **~L (8–12 dev-days)** |

### DECISIONS REQUIRED FROM THE USER

1. **eIDAS target scope.** Are we aiming for **PAdES-B-T / "advanced"** (RFC 3161
   trusted timestamp via any public TSA — fully covered by P1) — or do we
   eventually need **qualified** (QES + qualified TSU on the EU Trust List, paid
   QTSP)? This decides whether P3 + qualified-provider config is on the roadmap
   or explicitly out of scope.

2. **Default TSA provider in docs/SDK default.** FreeTSA.org (zero-friction) vs
   DigiCert (`http://timestamp.digicert.com`, trusted root, no account) as the
   documented default for `defaultTsaPost`. (The engine hardcodes none; this is
   the SDK's fallback URL + the README example.)

3. **Async signing is acceptable.** P1 makes the **timestamped** signing path
   `async` in the SDK (the existing sync `sign`/`signP12` stay sync for the
   non-timestamped case). Confirm the SDK may expose a `Promise`-returning
   `signTimestamped()` (it must, since the TSA round trip is network I/O — see
   §2/§A.3). The alternative (sync-XHR / Atomics) is rejected.

4. **ASN.1 fill: M1 (hand-roll TimeStampReq/resp with `sign/der.rs`) vs M2 (add
   the `tsp` crate).** Recommendation **M1** to preserve the "two narrow crypto
   exceptions" dependency posture; M2 only if/when P3 needs full TSTInfo
   verification.

5. **SSRF responsibility for the TSA URL.** Confirm the host owns the TSA-URL
   allow-list (the core only emits the request; the URL is host-supplied, exactly
   like the Google-Fonts host-pin model). The SDK's default `fetch` performs no
   allow-listing; consumers needing it pass `tsaFetch`.

6. **Scope of P1 PAdES coupling.** Ship P1 already emitting `ETSI.CAdES.detached`
   + signing-certificate-v2 (so the timestamped result is real PAdES-B-T) — vs.
   a narrower "RFC 3161 timestamp on the legacy `adbe.pkcs7.detached`" first.
   Recommendation: ship PAdES-B-T directly (the ESS attr is small and it's the
   standards-correct result).

---

## F. Risks & notes

- **`/Contents` sizing is the most common PAdES footgun.** A token + chain
  overflowing the fixed reservation silently corrupts the signature. P1 must
  size `/Contents` from the *actual* CMS length + slack, not a magic constant.
- **Two-phase consistency.** Phase 2 must finalize *exactly* the SignerInfo
  Phase 1 signed (only adding the unsigned TST attr). Keep all partial state in
  the core's pending object; never re-derive it in Phase 2.
- **TSA hash algorithm agreement.** We send SHA-256 in the MessageImprint;
  DigiCert/Sectigo/FreeTSA all support RSA-SHA256 tokens. Surface the TSA's
  chosen hash from the response for diagnostics.
- **Clock vs trusted time.** `/M` (`signing-time`) is a *claim*; the B-T
  timestamp is the *trusted* time. Don't conflate them in the API.
- **Verification is a separate epic.** This design covers *producing* B-T/LTV;
  *validating* third-party PAdES (chain building, OCSP/CRL checking, TSA token
  verification) is a distinct future workstream (where `tsp`/`x509-ocsp` pay off).
```
