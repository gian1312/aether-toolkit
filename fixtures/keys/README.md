# License-key fixtures (contract §10)

Structural goldens for the **license key format** described in
[`../../docs/CONTRACT.md`](../../docs/CONTRACT.md) §10. They exist so a change
to the v1/v2 byte layout also visibly breaks a fixture
([`../../tools/validate_fixtures.py`](../../tools/validate_fixtures.py) section
`(c)`).

> ## ⚠️ These are NOT valid license keys — by design
>
> A real key is the **Base58 encoding of a *signed* blob**, and signing requires
> the **vendor Ed25519 private seed**, which is deliberately **not** present in
> this public repository. So we do **not** — and cannot — mint real keys here.
>
> Instead, each `*.blob` file is the **decoded** blob (i.e. what you get *after*
> Base58-decoding) with its **64-byte signature region zero-filled**. That lets
> the validator check *structure* — the 84-vs-116 length discriminator, the
> payload field offsets, and the all-zero-fingerprint rule — **without ever
> verifying the signature** (which it can't, and shouldn't try to). Feeding any
> of these blobs to a real proprietary `aether_core` would fail signature
> verification. They authenticate nothing.

## Files

| File | Version | Length | Fingerprint | Meaning |
|------|---------|-------:|-------------|---------|
| `v1_unsigned.blob` | v1 | 84 B | — (n/a) | General / unlocked key (runs anywhere). |
| `v2_nodelock_unsigned.blob` | v2 | 116 B | non-zero | Node-locked to one machine. |
| `v2_unlocked_unsigned.blob` | v2 | 116 B | all-zero | v2 layout but **not** node-locked (runs anywhere). |

Regenerate them deterministically with
[`../../tools/make_fixtures.py`](../../tools/make_fixtures.py); validate with
`python3 tools/validate_fixtures.py`.

## Byte layout

Both versions are `payload || sig`, where `sig` is the trailing 64-byte Ed25519
signature over the payload. Integers are **big-endian**; day counts are measured
from the epoch **`2026-01-01`**. The **decoded length is the version
discriminator**: `84 ⇒ v1`, `116 ⇒ v2`, anything else ⇒ malformed.

**v1 — 84 bytes**

```
offset  size  field
  0       2   exp_days     (u16 big-endian)   payload[0:20]
  2       2   maint_days   (u16 big-endian)      │
  4      16   master_seed  (16 bytes)            │
 20      64   signature    (Ed25519, over payload[0:20])
```

**v2 — 116 bytes** (v1 payload + a 32-byte fingerprint)

```
offset  size  field
  0       2   exp_days     (u16 big-endian)   payload[0:52]
  2       2   maint_days   (u16 big-endian)      │
  4      16   master_seed  (16 bytes)            │
 20      32   fingerprint  SHA-256(machine_id)   │   all-zero => not node-locked
 52      64   signature    (Ed25519, over payload[0:52])
```

## Worked byte-offset example

These are the exact values baked into the fixtures (and asserted by the
validator), so the numbers below and the committed bytes cross-check.

* **`exp_days = 730`** → big-endian `0x02 0xDA` at `payload[0:2]`.
  `2026-01-01 + 730 days = 2028-01-01` (2026 and 2027 are 365-day years).
* **`maint_days = 365`** → big-endian `0x01 0x6D` at `payload[2:4]`.
  `2026-01-01 + 365 days = 2027-01-01`.
* **`master_seed`** at `payload[4:20]` = the 16 ASCII bytes `SEEDSEEDSEEDSEED`
  (`53 45 45 44 …`) — obviously synthetic; a real seed is random key material.
* **`fingerprint`** at `payload[20:52]` (v2 only) =
  `SHA-256("0123456789abcdef0123456789abcdef")` =
  `3eb1bd439947eb762998e566ccc2e099c791118b2f40579cc4f7da2b5061b7f9`.
  The **user-facing fingerprint** is exactly this 64-char lowercase hex — the
  string `aether_core --fingerprint` prints (contract §1.1). Here we hash a
  synthetic Linux-style `/etc/machine-id` as its exact bytes with no trailing
  newline; the precise `machine_id_string` bytes the engine hashes (newline/case
  handling, and the per-OS source in §10) are defined by `crypto.rs`, so treat
  this example as illustrative of the *layout*, not of any real host.
* **`signature`** = 64 × `0x00` (placeholder — see the warning above).

For `v2_unlocked_unsigned.blob` the fingerprint field is 32 × `0x00`, which per
§10 means the key is **not** node-locked and runs on any machine.

## What is *not* covered here

Signature generation and verification, expiry/maintenance enforcement, and the
runtime node-lock check (`SHA-256(local machine_id) == fingerprint`) all require
key material and a running proprietary engine, so they are **out of scope for
public fixtures**. They belong in private CI alongside a real keygen. See the
`docs/CONTRACT.md` §10 "TODO"-style caveat in `fixtures/README.md`.
