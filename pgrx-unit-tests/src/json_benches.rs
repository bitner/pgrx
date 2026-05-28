//LICENSE Portions Copyright 2019-2021 ZomboDB, LLC.
//LICENSE
//LICENSE Portions Copyright 2021-2023 Technology Concepts & Design, Inc.
//LICENSE
//LICENSE Portions Copyright 2023-2023 PgCentral Foundation, Inc. <contact@pgcentral.org>
//LICENSE
//LICENSE All rights reserved.
//LICENSE
//LICENSE Use of this source code is governed by the MIT license that can be found in the LICENSE file.

//! In-postgres benchmarks comparing the standard `JsonB` roundtrip cost against
//! an explicitly doubled text-roundtrip, for three representative payload sizes.
//!
//! # How to run
//!
//! ```text
//! cargo pgrx bench pgrx-unit-tests --features pg16,pg_bench
//! ```
//!
//! # Standard path (`bench_jsonb_standard_*`)
//!
//! The `JsonB` type uses a text-based roundtrip via the Postgres C functions
//! `jsonb_out` (decode) and `jsonb_in` (encode).  One call in each direction
//! per datum, mediated by `serde_json::Value` on the Rust heap:
//!
//! ```text
//! Postgres JSONB varlena
//!   → pg_detoast_datum_packed  (copy only when TOAST'd or compressed)
//!   → jsonb_out                (Postgres C fn: binary varlena → JSON text CStr)
//!   → serde_json::from_str     (JSON text → serde_json::Value)
//!   → serde_json::to_string    (serde_json::Value → JSON text String)
//!   → jsonb_in                 (Postgres C fn: JSON text CStr → binary varlena)
//! ```
//!
//! # Extra-text path (`bench_jsonb_extra_text_*`)
//!
//! The same standard decode, but the Rust side then explicitly serialises the
//! already-decoded `Value` back to a JSON text string and re-parses it before
//! calling `into_datum`.  This measures the overhead of that extra
//! serialization/deserialization round.
//!
//! # Memory profile (per roundtrip)
//!
//! Standard path allocations:
//!   1. detoasted varlena copy (only when TOAST'd/compressed)
//!   2. JSON text CStr from `jsonb_out`
//!   3. `serde_json::Value` tree on the Rust heap
//!   4. JSON text String from `serde_json::to_string`
//!   5. palloc'd result varlena via `jsonb_in`
//!
//! Extra-text path adds two allocations on top of the above:
//!   6. `String` from the extra `serde_json::to_string`   ← extra
//!   7. `serde_json::Value` from the extra `serde_json::from_str` ← extra
//!
//! So the extra-text path allocates roughly `2 × json_text_len` bytes more per
//! call than the standard path.
//!
//! # Zero-copy and TOAST
//!
//! True zero-copy from Postgres to Rust is **not** achievable through the
//! standard `JsonB` API today, for two independent reasons:
//!
//! 1. **TOAST decompression always copies.** `pg_detoast_datum_packed` returns
//!    the original pointer only when the datum is stored inline with the short
//!    1-byte varlena header and is neither compressed nor out-of-line.  Any
//!    larger or compressed datum triggers a fresh `palloc` copy before Rust
//!    even sees the bytes.
//!
//! 2. **Building a `serde_json::Value` allocates.** Even when detoast is a
//!    no-op the text must be parsed into a heap-allocated `Value` tree.
//!    Avoiding that would require a different lazy representation (e.g., a
//!    `RawJsonb` datum type exposing the text or binary slice directly), which
//!    is a separate future effort.

use pgrx::prelude::*;
use pgrx::JsonB;

// ---------------------------------------------------------------------------
// pg_extern workers
//
// These functions are always compiled when the `pg_bench` feature is active.
// They are distinct from the `jsonb_arg` / `jsonb_arg_via_text` helpers in
// json_tests.rs, which are only available under `pg_test`.
// ---------------------------------------------------------------------------

/// Identity roundtrip through the **standard** `JsonB` path.
///
/// Postgres calls `JsonB::from_polymorphic_datum` (decode via `jsonb_out`) and
/// then `JsonB::into_datum` (encode via `jsonb_in`) around this no-op, so the
/// benchmark measures the full standard decode + encode cost in a real Postgres
/// process.
#[pg_extern]
fn bench_jsonb_standard(json: JsonB) -> JsonB {
    json
}

/// Identity roundtrip with an **extra** Rust-side text serialization step.
///
/// The datum is decoded via the standard `jsonb_out` path (unavoidable), but
/// the `Value` is then serialised to text and re-parsed before being returned.
/// This measures the additional overhead of that extra serialization round
/// compared to `bench_jsonb_standard`.
#[pg_extern]
fn bench_jsonb_extra_text(json: JsonB) -> JsonB {
    let text = serde_json::to_string(&json.0).unwrap();
    JsonB(serde_json::from_str(&text).unwrap())
}

// ---------------------------------------------------------------------------
// Benchmark definitions
// ---------------------------------------------------------------------------

#[pg_schema]
mod benches {
    use pgrx::prelude::*;
    use pgrx::JsonB;
    use pgrx_bench::{BatchSize, Bencher, black_box};

    fn small_jsonb() -> JsonB {
        JsonB(serde_json::json!({
            "id": 42,
            "name": "Alice",
            "active": true,
            "score": 9.81
        }))
    }

    fn medium_jsonb() -> JsonB {
        JsonB(serde_json::json!({
            "user": {
                "id": 1234,
                "name": "Bob",
                "email": "bob@example.com",
                "roles": ["admin", "editor"],
                "address": {
                    "street": "123 Main St",
                    "city": "Anytown",
                    "zip": "12345",
                    "country": "US"
                }
            },
            "settings": {
                "theme": "dark",
                "notifications": true,
                "language": "en-US",
                "limits": { "max_uploads": 100, "max_size_mb": 50 }
            },
            "tags": ["premium", "verified"],
            "created_at": "2024-01-15T10:30:00Z"
        }))
    }

    fn large_jsonb() -> JsonB {
        let item = serde_json::json!({
            "user": {
                "id": 1234,
                "name": "Bob",
                "email": "bob@example.com",
                "roles": ["admin", "editor"],
                "address": {
                    "street": "123 Main St",
                    "city": "Anytown",
                    "zip": "12345",
                    "country": "US"
                }
            },
            "settings": {
                "theme": "dark",
                "notifications": true,
                "language": "en-US",
                "limits": { "max_uploads": 100, "max_size_mb": 50 }
            },
            "tags": ["premium", "verified"],
            "created_at": "2024-01-15T10:30:00Z"
        });
        JsonB(serde_json::Value::Array((0..100).map(|_| item.clone()).collect()))
    }

    // -- Standard path --

    #[pg_bench]
    fn bench_jsonb_standard_small(b: &mut Bencher) {
        b.iter_batched(
            small_jsonb,
            |v| black_box(super::bench_jsonb_standard(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    #[pg_bench]
    fn bench_jsonb_standard_medium(b: &mut Bencher) {
        b.iter_batched(
            medium_jsonb,
            |v| black_box(super::bench_jsonb_standard(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    #[pg_bench]
    fn bench_jsonb_standard_large(b: &mut Bencher) {
        b.iter_batched(
            large_jsonb,
            |v| black_box(super::bench_jsonb_standard(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    // -- Extra-text path (measures overhead of an additional Rust-side text roundtrip) --

    #[pg_bench]
    fn bench_jsonb_extra_text_small(b: &mut Bencher) {
        b.iter_batched(
            small_jsonb,
            |v| black_box(super::bench_jsonb_extra_text(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    #[pg_bench]
    fn bench_jsonb_extra_text_medium(b: &mut Bencher) {
        b.iter_batched(
            medium_jsonb,
            |v| black_box(super::bench_jsonb_extra_text(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    #[pg_bench]
    fn bench_jsonb_extra_text_large(b: &mut Bencher) {
        b.iter_batched(
            large_jsonb,
            |v| black_box(super::bench_jsonb_extra_text(black_box(v))),
            BatchSize::SmallInput,
        );
    }
}
