# 0011 — Crypto advisories without a compatible fix

Status: accepted (2026-11-07). The project owner accepted the residual risks below.

## Context

`cargo deny` reports five advisories in crypto crates. None of them has a fix that is compatible with the current dependency graph. This ADR records why each one is still in `rust/deny.toml` and what would end the exception. A passing `cargo deny` run means these decisions are on record. It does not mean the advisories are fixed.

DAVE end-to-end encryption runs through `davey` 0.1.5 from crates.io (not vendored; the vendored `discord-stream-rs` depends on it). It creates `OpenMlsRustCrypto`, which uses the DAVEv1 suite: P256 signatures, P256 HPKE, AES-128-GCM and SHA-256. `HpkeLibcrux` is in the lockfile but not in the normal build graph.

## Decision

| Advisory | Where it is reached | What we do | Ends when |
|---|---|---|---|
| RUSTSEC-2023-0071, `rsa` 0.9 (Marvin timing side channel) | QR login (`rust/crates/dstreamy-discord/src/remote_auth.rs`) uses RSA-OAEP to decrypt the nonce, the user payload and the final token. Token login and browser login never use RSA. | Each QR login uses a new 2048-bit key. Decryption is blinded with `OsRng`. Each key decrypts at most 3 ciphertexts. The gateway can send HELLO and `nonce_proof` only once each. A session lasts at most 5 minutes locally, even if the gateway allows longer. **Accepted:** blinding protects against simple timing measurements but is not a full Marvin fix. An attacker would need control of the remote-auth gateway's ciphertexts plus precise timing measurements, and gets at most 3 measurements per key. | `rsa` releases a constant-time decryption, or QR login moves to a reviewed RSA implementation |
| RUSTSEC-2026-0387, `crypto-bigint` 0.5 | The P256 private key is used in `crypto-bigint` arithmetic in two places: MLS credential signing (`openmls_basic_credential` → `ecdsa` → `p256`) and P256 HPKE key exchange. | No change in this project. `crypto-bigint` ≥ 0.7.6 does not work with `elliptic-curve` 0.13, and lockfile changes alone cannot fix this. **Accepted:** a leak would depend on the CPU and the compiler. No practical leak has been shown for this code. | `openmls`, `openmls_rust_crypto`, `davey` and the RustCrypto stack release versions built on `elliptic-curve` 0.14; then update and remove the exception |
| RUSTSEC-2026-0212, `libcrux-secrets` 0.0.5 | Only through the `libcrux-traits` digest adapter. The affected aarch64 `Select`/`Swap` code is not called on the P256 path. | Exception based on reachability: the affected code is not used. | A `davey`/`hpke-rs` release built on `libcrux` ≥ 0.0.6, or any change that puts the affected code on the DAVE path (that would make this exception invalid) |
| RUSTSEC-2026-0207, `libcrux-sha3` 0.0.8 | The affected repeated incremental squeeze API is not used. HPKE calls SHAKE only for X-Wing and ML-KEM, not for the P256 suite, and those calls are one-shot. | Exception based on reachability. | `libcrux-sha3` ≥ 0.0.10 arrives with a compatible `hpke-rs`/`davey` release |
| RUSTSEC-2026-0208, `libcrux-sha3` 0.0.8 | The affected AVX2x4 SHAKE path is not used. The calls use the portable code with output sizes that are safe. | Exception based on reachability. | Same as above |

## Consequences

- Any dependency change that changes `davey`, `openmls_rust_crypto`, `hpke-rs` or the DAVE suite must check this table again. A reachability exception can become invalid without warning.
- QR login stays available. A user who wants no RSA exposure can log in with the browser or a token instead.
- The evidence for this ADR was the source review of the graph at that time: `rsa` `src/key.rs`, `ecdsa` `src/hazmat.rs`, `p256` `src/arithmetic/scalar.rs`, `crypto-bigint` `src/uint/add_mod.rs`, `hpke-rs` `src/kem.rs` and `libcrux-sha3` `src/lib.rs`.
