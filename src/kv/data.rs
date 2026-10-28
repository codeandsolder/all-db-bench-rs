use rand::{RngExt, rngs::SmallRng};

use super::types::{AccessPattern, KeyShape, ValuePattern, Workload};

pub(crate) fn mix_u64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

pub(crate) fn key(id: u64, len: usize, shape: KeyShape, seed: u64) -> Vec<u8> {
    debug_assert!(len >= 8);
    let mut out = vec![0u8; len];
    match shape {
        KeyShape::Sequential => {
            out[..8].copy_from_slice(&id.to_be_bytes());
            let mut x = mix_u64(id ^ seed ^ 0x6b65_792d_7365_7100);
            for chunk in out[8..].chunks_mut(8) {
                x = mix_u64(x.wrapping_add(0x9e37_79b9_7f4a_7c15));
                let b = x.to_le_bytes();
                chunk.copy_from_slice(&b[..chunk.len()]);
            }
        }
        KeyShape::SharedPrefix => {
            out[..len - 8].fill(0x5a);
            out[len - 8..].copy_from_slice(&id.to_be_bytes());
        }
        KeyShape::Hashed => {
            let mut x = mix_u64(id ^ seed ^ 0x6861_7368_6564_6b79);
            out[..8].copy_from_slice(&x.to_be_bytes());
            for chunk in out[8..].chunks_mut(8) {
                x = mix_u64(x.wrapping_add(0x9e37_79b9_7f4a_7c15));
                let b = x.to_le_bytes();
                chunk.copy_from_slice(&b[..chunk.len()]);
            }
        }
    }
    out
}

pub(crate) fn sample_existing_id(
    rng: &mut SmallRng,
    records: u64,
    pattern: AccessPattern,
    workload: Workload,
) -> u64 {
    let effective = match pattern {
        AccessPattern::Auto => match workload {
            Workload::ReadHeavy | Workload::Balanced => AccessPattern::Hot80,
            _ => AccessPattern::Uniform,
        },
        other => other,
    };
    let (hot_percent, hot_fraction) = match effective {
        AccessPattern::Auto | AccessPattern::Uniform => return rng.random_range(0..records),
        AccessPattern::Hot80 => (80u32, 5u64),
        AccessPattern::Hot95 => (95u32, 20u64),
    };
    if records < hot_fraction {
        return rng.random_range(0..records);
    }
    let hot = (records / hot_fraction).max(1);
    if rng.random_range(0..100u32) < hot_percent {
        rng.random_range(0..hot)
    } else if hot < records {
        rng.random_range(hot..records)
    } else {
        rng.random_range(0..records)
    }
}

pub(crate) fn sample_read_id(
    rng: &mut SmallRng,
    records: u64,
    ops: u64,
    miss_percent: u8,
    access_pattern: AccessPattern,
    workload: Workload,
) -> (u64, bool) {
    let miss = miss_percent > 0 && rng.random_range(0..100u32) < u32::from(miss_percent);
    if miss {
        let miss_base = records.saturating_add(ops).saturating_add(1_000_000);
        let span = records.max(1);
        (miss_base.saturating_add(rng.random_range(0..span)), false)
    } else {
        (
            sample_existing_id(rng, records, access_pattern, workload),
            true,
        )
    }
}

pub(crate) fn value(id: u64, len: usize, salt: u64, pattern: ValuePattern) -> Vec<u8> {
    match pattern {
        ValuePattern::Zeros => vec![0u8; len],
        ValuePattern::Repeated => {
            let block = mix_u64(id ^ salt ^ 0x7265_7065_6174_6564).to_le_bytes();
            let mut out = vec![0u8; len];
            for chunk in out.chunks_mut(block.len()) {
                chunk.copy_from_slice(&block[..chunk.len()]);
            }
            out
        }
        ValuePattern::StructuredBinary => {
            let mut out = vec![0u8; len];
            let mut x = mix_u64(id ^ salt ^ 0x7374_7275_6374_6269);
            for (block_index, chunk) in out.chunks_mut(64).enumerate() {
                let mut block = [0u8; 64];
                block[..16].copy_from_slice(b"DBBENCH-STRUCTv1");
                block[16..24].copy_from_slice(&id.to_le_bytes());
                block[24..32].copy_from_slice(&(block_index as u64).to_le_bytes());
                x = mix_u64(x.wrapping_add(block_index as u64));
                block[32..40].copy_from_slice(&x.to_le_bytes());
                block[40..].fill((block_index % 7) as u8);
                chunk.copy_from_slice(&block[..chunk.len()]);
            }
            out
        }
        ValuePattern::StructuredText => {
            let seed = mix_u64(id ^ salt ^ 0x7374_7275_6374_7478);
            let record = format!(
                r#"{{"kind":"dbbench","version":1,"id":{id},"region":"r{:02}","state":"active","counter":{},"message":"repeatable structured payload for compression testing"}}"#,
                id % 8,
                seed % 100_000,
            );
            let bytes = record.as_bytes();
            let mut out = vec![0u8; len];
            for chunk in out.chunks_mut(bytes.len()) {
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            out
        }
        ValuePattern::PseudoRandom => {
            let mut x = id ^ salt ^ 0x9e37_79b9_7f4a_7c15;
            let mut out = vec![0u8; len];
            for chunk in out.chunks_mut(8) {
                x = mix_u64(x);
                let b = x.to_le_bytes();
                let n = chunk.len();
                chunk.copy_from_slice(&b[..n]);
                x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_patterns_are_sized_and_deterministic() {
        let patterns = [
            ValuePattern::PseudoRandom,
            ValuePattern::Zeros,
            ValuePattern::Repeated,
            ValuePattern::StructuredBinary,
            ValuePattern::StructuredText,
        ];
        for pattern in patterns {
            let a = value(42, 4096, 7, pattern);
            let b = value(42, 4096, 7, pattern);
            assert_eq!(a.len(), 4096);
            assert_eq!(a, b);
        }
        assert!(
            value(42, 257, 7, ValuePattern::Zeros)
                .iter()
                .all(|&x| x == 0)
        );
        assert_ne!(
            value(42, 4096, 7, ValuePattern::PseudoRandom),
            value(42, 4096, 7, ValuePattern::StructuredBinary)
        );
        assert!(
            String::from_utf8(value(42, 4096, 7, ValuePattern::StructuredText))
                .expect("structured text pattern must be UTF-8")
                .contains("dbbench")
        );
    }
}
