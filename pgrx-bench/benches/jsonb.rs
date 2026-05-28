// Pure-Rust benchmarks comparing the `jsonb` crate's binary encode/decode
// against plain JSON text serialization via `serde_json`.
//
// These benchmarks intentionally do NOT require a running Postgres instance.
// They measure only pure-Rust conversion work for three representative payload
// sizes:
//
//   - small:  a flat object with a handful of scalar fields
//   - medium: a nested object several levels deep with mixed types
//   - large:  an array of 100 medium-sized objects
//
// ⚠️  NOTE ON FORMAT COMPATIBILITY
// The `jsonb` crate (by Datafuse Labs) uses a binary format *inspired by*
// PostgreSQL's JSONB but is NOT bit-for-bit compatible with it.  In
// particular the container type constants differ:
//
//   PostgreSQL (pg16):     JB_FOBJECT = 0x20000000,  JB_FARRAY = 0x40000000
//   jsonb crate (v0.5.6):  object     = 0x40000000,  array     = 0x80000000
//
// As a result, bytes produced by `jsonb::Value::to_vec()` cannot be fed
// directly into a Postgres JSONB varlena, and raw Postgres JSONB bytes
// cannot be parsed by `jsonb::from_raw_jsonb`.  The in-Postgres `JsonB`
// type therefore uses the text-based `jsonb_out`/`jsonb_in` C functions
// instead of this crate.
//
// These benchmarks are nonetheless useful for comparing the relative cost
// of the `jsonb` crate's custom binary format vs. plain JSON text, without
// Postgres overhead.
//
// Run with:
//   cargo bench -p pgrx-bench --bench jsonb
//
// Or compare against a saved baseline:
//   cargo bench -p pgrx-bench --bench jsonb -- --save-baseline old
//   # (apply changes)
//   cargo bench -p pgrx-bench --bench jsonb -- --baseline old
//
// For in-postgres benchmarks that measure the full Postgres roundtrip
// (palloc, detoast, C function call overhead), see
// pgrx-unit-tests/src/json_benches.rs and run:
//   cargo pgrx bench pgrx-unit-tests --features pg16,pg_bench
//
// ── Memory usage ────────────────────────────────────────────────────────────
// The `bench_sizes` group below prints the encoded byte sizes for each path.
// This acts as a lower-bound proxy for per-call memory.
//
// Typical observation:
//   Binary size ≈ text size for most workloads.  The main difference is in
//   *allocations*: the binary path avoids intermediate UTF-8 Strings, while
//   the text path allocates one String per encode and one per decode.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Payload fixtures
// ---------------------------------------------------------------------------

fn small_payload() -> Value {
    json!({
        "id": 42,
        "name": "Alice",
        "active": true,
        "score": 9.81
    })
}

fn medium_payload() -> Value {
    json!({
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
            "limits": {
                "max_uploads": 100,
                "max_size_mb": 50
            }
        },
        "tags": ["premium", "verified"],
        "created_at": "2024-01-15T10:30:00Z"
    })
}

fn large_payload() -> Value {
    let item = medium_payload();
    Value::Array((0..100).map(|_| item.clone()).collect())
}

// ---------------------------------------------------------------------------
// Encode benchmarks (serde_json::Value → serialized bytes)
// ---------------------------------------------------------------------------

/// New path: serde_json::Value → jsonb::Value → binary bytes
fn encode_binary(v: &Value) -> Vec<u8> {
    let jv = jsonb::Value::from(v);
    jv.to_vec()
}

/// Old path: serde_json::Value → JSON text string
fn encode_text(v: &Value) -> String {
    serde_json::to_string(v).unwrap()
}

fn bench_encode(c: &mut Criterion) {
    let payloads: &[(&str, Value)] = &[
        ("small", small_payload()),
        ("medium", medium_payload()),
        ("large", large_payload()),
    ];

    let mut group = c.benchmark_group("jsonb_encode");

    for (name, payload) in payloads {
        group.bench_with_input(BenchmarkId::new("binary", name), payload, |b, v| {
            b.iter(|| encode_binary(black_box(v)))
        });
        group.bench_with_input(BenchmarkId::new("text", name), payload, |b, v| {
            b.iter(|| encode_text(black_box(v)))
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Decode benchmarks (bytes → serde_json::Value)
// ---------------------------------------------------------------------------

/// New path: binary jsonb bytes → serde_json::Value
fn decode_binary(bytes: &[u8]) -> Value {
    let raw = jsonb::RawJsonb::new(bytes);
    jsonb::from_raw_jsonb::<Value>(&raw).unwrap()
}

/// Old path: JSON text string → serde_json::Value
fn decode_text(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

fn bench_decode(c: &mut Criterion) {
    // Pre-encode each payload once so the decode benchmarks only measure decoding.
    let payloads: &[(&str, Value)] = &[
        ("small", small_payload()),
        ("medium", medium_payload()),
        ("large", large_payload()),
    ];

    let binary_payloads: Vec<(&str, Vec<u8>)> = payloads
        .iter()
        .map(|(name, v)| (*name, encode_binary(v)))
        .collect();

    let text_payloads: Vec<(&str, String)> = payloads
        .iter()
        .map(|(name, v)| (*name, encode_text(v)))
        .collect();

    let mut group = c.benchmark_group("jsonb_decode");

    for (name, bytes) in &binary_payloads {
        group.bench_with_input(BenchmarkId::new("binary", name), bytes.as_slice(), |b, bytes| {
            b.iter(|| decode_binary(black_box(bytes)))
        });
    }

    for (name, text) in &text_payloads {
        group.bench_with_input(BenchmarkId::new("text", name), text.as_str(), |b, text| {
            b.iter(|| decode_text(black_box(text)))
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Full roundtrip benchmarks (encode + decode together)
// ---------------------------------------------------------------------------

fn bench_roundtrip(c: &mut Criterion) {
    let payloads: &[(&str, Value)] = &[
        ("small", small_payload()),
        ("medium", medium_payload()),
        ("large", large_payload()),
    ];

    let mut group = c.benchmark_group("jsonb_roundtrip");

    for (name, payload) in payloads {
        group.bench_with_input(BenchmarkId::new("binary", name), payload, |b, v| {
            b.iter(|| {
                let bytes = encode_binary(black_box(v));
                decode_binary(black_box(&bytes))
            })
        });
        group.bench_with_input(BenchmarkId::new("text", name), payload, |b, v| {
            b.iter(|| {
                let text = encode_text(black_box(v));
                decode_text(black_box(&text))
            })
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Encoded-size benchmarks
//
// These are not timing benchmarks: they report the number of bytes produced
// by each encoding strategy as a proxy for minimum per-call memory usage.
// A single iteration runs instantly; Criterion is used here purely so the
// sizes appear alongside the other benchmark output.
// ---------------------------------------------------------------------------

fn bench_sizes(c: &mut Criterion) {
    let payloads: &[(&str, Value)] = &[
        ("small", small_payload()),
        ("medium", medium_payload()),
        ("large", large_payload()),
    ];

    let mut group = c.benchmark_group("jsonb_encoded_bytes");

    // Emit a single no-op iteration just to record the size via a side-channel
    // print.  Criterion does not have a native "report a scalar" API so we
    // print the comparison once and run a trivial timing loop.
    for (name, payload) in payloads {
        let binary_bytes = encode_binary(payload).len();
        let text_bytes = encode_text(payload).len();

        // Print sizes to stdout so they appear in `cargo bench` output.
        println!(
            "jsonb_encoded_bytes/{name}: binary={binary_bytes}B  text={text_bytes}B  \
             ratio={:.2}",
            binary_bytes as f64 / text_bytes as f64
        );

        group.bench_with_input(BenchmarkId::new("binary", name), payload, |b, v| {
            b.iter(|| encode_binary(black_box(v)).len())
        });
        group.bench_with_input(BenchmarkId::new("text", name), payload, |b, v| {
            b.iter(|| encode_text(black_box(v)).len())
        });
    }

    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode, bench_roundtrip, bench_sizes);
criterion_main!(benches);
