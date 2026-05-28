//LICENSE Portions Copyright 2019-2021 ZomboDB, LLC.
//LICENSE
//LICENSE Portions Copyright 2021-2023 Technology Concepts & Design, Inc.
//LICENSE
//LICENSE Portions Copyright 2023-2023 PgCentral Foundation, Inc. <contact@pgcentral.org>
//LICENSE
//LICENSE All rights reserved.
//LICENSE
//LICENSE Use of this source code is governed by the MIT license that can be found in the LICENSE file.

//! In-postgres benchmarks comparing the binary JSONB decode/encode path (current) against the
//! old text-based roundtrip for three representative payload sizes.
//!
//! # How to run
//!
//! ```text
//! cargo pgrx bench pgrx-unit-tests --features pg16,pg_bench
//! ```
//!
//! # Binary path (`bench_jsonb_binary_*`)
//!
//! ```text
//! Postgres binary JSONB varlena
//!   → pg_detoast_datum_packed       (copy only when TOAST'd or compressed)
//!   → jsonb::from_raw_jsonb::<Value> (binary parse, no text conversion)
//!   → serde_json::Value
//!   → jsonb::Value::to_vec           (binary encode)
//!   → palloc + memcpy into result varlena
//! ```
//!
//! # Text path (`bench_jsonb_text_*`)
//!
//! The same binary varlena input, but the Rust side explicitly serialises the
//! already-decoded `Value` back to a JSON text string and re-parses it,
//! reproducing the extra text-conversion work of the old code path.
//!
//! # Memory profile (per roundtrip)
//!
//! Binary path allocations:
//!   1. detoasted varlena copy (only when TOAST'd/compressed)
//!   2. `serde_json::Value` tree on the Rust heap
//!   3. `Vec<u8>` from `jsonb::Value::to_vec`
//!   4. palloc'd result varlena
//!
//! Text path adds two extra allocations on top of the above:
//!   5. `String` from `serde_json::to_string`   ← extra
//!   6. `String` from `serde_json::from_str`    ← extra
//!
//! So the text path allocates roughly `2 × json_text_len` bytes extra per call.
//! For typical small-to-medium payloads that is hundreds to a few thousand extra
//! bytes per call, held live simultaneously before the first drops.
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
//!    no-op the binary bytes must be walked and decoded into a heap-allocated
//!    `Value` tree.  Avoiding that would require a different lazy representation
//!    (e.g., a `RawJsonb` datum type exposing the binary slice directly), which
//!    is a separate future effort.
//!
//! The binary path eliminates the extra intermediate text string copies that the
//! old code path required, but it does not achieve zero-copy.

use pgrx::prelude::*;
use pgrx::JsonB;

// ---------------------------------------------------------------------------
// pg_extern workers
//
// These functions are always compiled when the `pg_bench` feature is active.
// They are distinct from the `jsonb_arg` / `jsonb_arg_via_text` helpers in
// json_tests.rs, which are only available under `pg_test`.
// ---------------------------------------------------------------------------

/// Identity roundtrip through the **binary** path (current implementation).
///
/// Postgres calls `JsonB::from_polymorphic_datum` (binary decode) and then
/// `JsonB::into_datum` (binary encode) around this no-op, so the benchmark
/// measures the full binary decode + encode cost in a real Postgres process.
#[pg_extern]
fn bench_jsonb_binary(json: JsonB) -> JsonB {
    json
}

/// Identity roundtrip forced through the **text** path for comparison.
///
/// The datum is received via the binary decode path (unavoidable), but the
/// `Value` is then serialised to text and re-parsed before being returned,
/// reproducing the extra work performed by the pre-binary code path.
#[pg_extern]
fn bench_jsonb_text(json: JsonB) -> JsonB {
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

    // -- Binary path --

    #[pg_bench]
    fn bench_jsonb_binary_small(b: &mut Bencher) {
        b.iter_batched(
            small_jsonb,
            |v| black_box(super::bench_jsonb_binary(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    #[pg_bench]
    fn bench_jsonb_binary_medium(b: &mut Bencher) {
        b.iter_batched(
            medium_jsonb,
            |v| black_box(super::bench_jsonb_binary(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    #[pg_bench]
    fn bench_jsonb_binary_large(b: &mut Bencher) {
        b.iter_batched(
            large_jsonb,
            |v| black_box(super::bench_jsonb_binary(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    // -- Text path (comparison baseline) --

    #[pg_bench]
    fn bench_jsonb_text_small(b: &mut Bencher) {
        b.iter_batched(
            small_jsonb,
            |v| black_box(super::bench_jsonb_text(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    #[pg_bench]
    fn bench_jsonb_text_medium(b: &mut Bencher) {
        b.iter_batched(
            medium_jsonb,
            |v| black_box(super::bench_jsonb_text(black_box(v))),
            BatchSize::SmallInput,
        );
    }

    #[pg_bench]
    fn bench_jsonb_text_large(b: &mut Bencher) {
        b.iter_batched(
            large_jsonb,
            |v| black_box(super::bench_jsonb_text(black_box(v))),
            BatchSize::SmallInput,
        );
    }
}
