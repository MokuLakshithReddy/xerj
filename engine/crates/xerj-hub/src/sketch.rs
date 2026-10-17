//! Deterministic Bloom filter over a corpus's terms — the catalogue's offline
//! router hint (`docs/CORPUS_HUB_V2_IMPLEMENTATION.md`, step 4, "Term sketch").
//!
//! The catalogue carries one [`Sketch`] per corpus so the client can rule a
//! corpus out for a question without downloading its pack: a term the question
//! mentions that the sketch reports absent is *certainly* not in that corpus.
//! A Bloom filter answers "possibly present" (with a bounded false-positive
//! rate) or "certainly absent" — never a false negative — which is exactly the
//! guarantee the router needs.
//!
//! The parameters are fixed by the plan so that every builder produces
//! byte-identical `bits_base64` for the same set of terms:
//!
//! - Collect the distinct terms first; `m_bits = 8 · max(1024, 10 · distinct)`,
//!   rounded up to a multiple of 64.
//! - `k_hashes = 7`.
//! - Double hashing (Kirsch–Mitzenmacher): `h1 = FNV-1a 64(term)`,
//!   `h2 = FNV-1a 64(term's bytes reversed)`, and probe `i` is
//!   `(h1 + i · h2) mod m_bits`.
//!
//! FNV-1a is implemented here rather than pulled from a hashing crate so the
//! hash stays pinned to this on-the-wire contract and never drifts with a
//! dependency bump.

use std::collections::HashSet;

use base64::Engine as _;
use xerj_common::hub::Sketch;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Number of probes per term. Fixed by the plan.
const K_HASHES: u32 = 7;

/// FNV-1a 64 over a byte sequence.
fn fnv1a64<'a, I: IntoIterator<Item = &'a u8>>(bytes: I) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// `(h1, h2)` for a term: FNV-1a 64 of its bytes, and of its bytes reversed.
fn hashes(term: &str) -> (u64, u64) {
    let h1 = fnv1a64(term.as_bytes());
    let h2 = fnv1a64(term.as_bytes().iter().rev());
    (h1, h2)
}

/// The `i`-th probe bit index for a term: `(h1 + i · h2) mod m_bits`.
/// `m_bits` is always non-zero here, so the modulo never divides by zero.
#[inline]
fn probe(h1: u64, h2: u64, i: u32, m_bits: u64) -> u64 {
    h1.wrapping_add(u64::from(i).wrapping_mul(h2)) % m_bits
}

/// Round `n` up to the next multiple of 64.
fn round_up_to_64(n: u64) -> u64 {
    n.div_ceil(64).saturating_mul(64)
}

/// `m_bits` for a corpus of `distinct` distinct terms: `8 · max(1024, 10 ·
/// distinct)`, rounded up to a multiple of 64 (so it is always a whole number
/// of bytes, and at least `8 · 1024`).
fn sizing(distinct: u64) -> u64 {
    let load = distinct.saturating_mul(10).max(1024);
    round_up_to_64(load.saturating_mul(8))
}

/// Build a term sketch from an iterator of terms (distinct terms are what counts).
pub fn build<I: IntoIterator<Item = String>>(terms: I) -> Sketch {
    let distinct: HashSet<String> = terms.into_iter().collect();
    let m_bits = sizing(distinct.len() as u64);

    // m_bits is a multiple of 64, so this is exact and bit indices stay in bounds.
    let mut bits = vec![0u8; m_bits.div_ceil(8) as usize];
    for term in &distinct {
        let (h1, h2) = hashes(term);
        for i in 0..K_HASHES {
            let bit = probe(h1, h2, i, m_bits);
            bits[(bit / 8) as usize] |= 1u8 << (bit % 8);
        }
    }

    Sketch {
        m_bits: m_bits as u32,
        k_hashes: K_HASHES,
        bits_base64: base64::engine::general_purpose::STANDARD.encode(&bits),
    }
}

/// Is `term` possibly present? (Bloom: false is certain, true may be a false positive.)
pub fn contains(sketch: &Sketch, term: &str) -> bool {
    let m_bits = u64::from(sketch.m_bits);
    if m_bits == 0 {
        return false;
    }
    let Ok(bits) = base64::engine::general_purpose::STANDARD.decode(sketch.bits_base64.as_bytes())
    else {
        return false;
    };

    let (h1, h2) = hashes(term);
    for i in 0..sketch.k_hashes {
        let bit = probe(h1, h2, i, m_bits);
        let byte = (bit / 8) as usize;
        // A missing or unset bit means the term was certainly never inserted.
        if byte >= bits.len() || bits[byte] & (1u8 << (bit % 8)) == 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic PRNG so the "random" probe terms are reproducible
    /// in CI (no `rand` dependency). Seed is non-zero, as xorshift requires.
    fn xorshift64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    #[test]
    fn every_inserted_term_is_present() {
        let terms: Vec<String> = (0..1000).map(|i| format!("member_{i}")).collect();
        let sketch = build(terms.clone());
        for term in &terms {
            assert!(
                contains(&sketch, term),
                "inserted term {term} reported absent"
            );
        }
    }

    #[test]
    fn false_positive_rate_under_two_percent() {
        let terms: Vec<String> = (0..1000).map(|i| format!("member_{i}")).collect();
        let sketch = build(terms);

        // 16-char hex strings never contain '_', so none can equal a "member_i"
        // term — every probe below is a genuine non-member.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let trials = 10_000u32;
        let mut false_positives = 0u32;
        for _ in 0..trials {
            let candidate = format!("{:016x}", xorshift64(&mut state));
            if contains(&sketch, &candidate) {
                false_positives += 1;
            }
        }
        let rate = f64::from(false_positives) / f64::from(trials);
        // Visible with `--nocapture`; a Bloom filter sized at 8 · 10 bits per
        // term keeps this far below the 2% ceiling.
        eprintln!("sketch false-positive rate: {false_positives}/{trials} = {rate:.4}");
        assert!(
            rate < 0.02,
            "false-positive rate {rate} ({false_positives}/{trials}) exceeds 2%"
        );
    }

    #[test]
    fn deterministic_across_insertion_order() {
        let a = vec![
            "alpha".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
            "delta".to_string(),
            "epsilon".to_string(),
        ];
        let mut b = a.clone();
        b.reverse();
        b.rotate_left(2);

        let sa = build(a);
        let sb = build(b);
        assert_eq!(
            sa.bits_base64, sb.bits_base64,
            "order changed the bit array"
        );
        assert_eq!(sa.m_bits, sb.m_bits);
        assert_eq!(sa.k_hashes, sb.k_hashes);
    }

    #[test]
    fn duplicate_terms_do_not_change_the_sketch() {
        // Same distinct set, one copy heavily duplicated → identical sketch.
        let unique = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let mut dupes = unique.clone();
        for _ in 0..50 {
            dupes.extend(unique.clone());
        }
        assert_eq!(build(unique).bits_base64, build(dupes).bits_base64);
    }

    #[test]
    fn m_bits_is_multiple_of_64_and_at_least_8192() {
        for n in [0usize, 1, 103, 500, 2000] {
            let terms: Vec<String> = (0..n).map(|i| format!("t{i}")).collect();
            let sketch = build(terms);
            assert_eq!(
                sketch.m_bits % 64,
                0,
                "m_bits {} not a 64-multiple",
                sketch.m_bits
            );
            assert!(
                sketch.m_bits >= 8 * 1024,
                "m_bits {} below floor",
                sketch.m_bits
            );
            assert_eq!(sketch.k_hashes, 7);
            // bits_base64 decodes to exactly ceil(m_bits/8) bytes.
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(sketch.bits_base64.as_bytes())
                .expect("sketch holds valid base64");
            assert_eq!(bytes.len(), (sketch.m_bits as usize).div_ceil(8));
        }
    }

    #[test]
    fn empty_corpus_has_floor_sizing_and_absent_everything() {
        let sketch = build(Vec::<String>::new());
        assert_eq!(sketch.m_bits, 8 * 1024);
        assert!(!contains(&sketch, "anything"));
    }

    #[test]
    fn large_corpus_uses_ten_bits_per_term_branch() {
        // 2000 distinct: 8 · max(1024, 20000) = 160000, already a 64-multiple.
        let terms: Vec<String> = (0..2000).map(|i| format!("sym_{i}")).collect();
        let sketch = build(terms);
        assert_eq!(sketch.m_bits, 8 * 20_000);
    }

    #[test]
    fn fnv1a64_matches_known_vectors() {
        // Canonical FNV-1a 64 test vectors.
        assert_eq!(fnv1a64(b"".iter()), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a".iter()), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar".iter()), 0x8594_4171_f739_67e8);
    }
}
