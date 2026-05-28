// Benchmarks comparing the binary JSONB encode/decode path (new) against the text-based
// roundtrip path (old, using serde_json text serialization).
//
// These benchmarks intentionally do NOT require a running Postgres instance.  They measure only
// the pure-Rust conversion work — the part that changed — for three representative payload sizes:
//
//   - small:  a flat object with a handful of scalar fields
//   - medium: a nested object several levels deep with mixed types
//   - large:  an array of 100 medium-sized objects
//
// Run with:
//   cargo bench -p pgrx-bench --bench jsonb
//
// Or compare against a saved baseline:
//   cargo bench -p pgrx-bench --bench jsonb -- --save-baseline old
//   # (apply changes)
//   cargo bench -p pgrx-bench --bench jsonb -- --baseline old

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

criterion_group!(benches, bench_encode, bench_decode, bench_roundtrip);
criterion_main!(benches);
