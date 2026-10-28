use clap::ValueEnum;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadPattern {
    HexRandom,
    RandomAscii,
    Zeros,
    Repeated,
    StructuredText,
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

#[cfg(test)]
fn legacy_payload(id: u64, len: usize, salt: u64) -> String {
    payload_for(PayloadPattern::HexRandom, id, len, salt)
}

pub fn payload_for(pattern: PayloadPattern, id: u64, len: usize, salt: u64) -> String {
    let mut x = id ^ salt ^ 0x9e37_79b9_7f4a_7c15;
    match pattern {
        PayloadPattern::HexRandom => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut out = String::with_capacity(len);
            for _ in 0..len {
                x = mix(x);
                out.push(char::from(HEX[(x as usize) & 15]));
            }
            out
        }
        PayloadPattern::RandomAscii => {
            let mut out = String::with_capacity(len);
            for _ in 0..len {
                x = mix(x);
                out.push(char::from(32 + (x % 95) as u8));
            }
            out
        }
        PayloadPattern::Zeros => "0".repeat(len),
        PayloadPattern::Repeated => {
            let mut block = [b' '; 8];
            for byte in &mut block {
                x = mix(x);
                *byte = 32 + (x % 95) as u8;
            }
            let mut out = String::with_capacity(len);
            for index in 0..len {
                out.push(char::from(block[index % block.len()]));
            }
            out
        }
        PayloadPattern::StructuredText => {
            let mut out = String::with_capacity(len);
            let mut sequence = 0u64;
            while out.len() < len {
                x = mix(x);
                let segment = format!(
                    "{{\"schema\":\"telemetry-v1\",\"device\":\"dev-{id:08x}\",\"region\":\"eu-central\",\"status\":\"ok\",\"seq\":{sequence},\"reading\":\"{x:016x}\"}}\n"
                );
                let remaining = len - out.len();
                let take = remaining.min(segment.len());
                out.push_str(&segment[..take]);
                sequence = sequence.wrapping_add(1);
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PayloadPattern, legacy_payload, payload_for};

    #[test]
    fn legacy_hex_payload_is_stable() {
        assert_eq!(
            legacy_payload(42, 32, 0x5eed_2026),
            "0d2dea86df7ce7523c76c4190f7f418b"
        );
    }

    #[test]
    fn patterns_are_exact_ascii_bytes_and_deterministic() {
        for pattern in [
            PayloadPattern::HexRandom,
            PayloadPattern::RandomAscii,
            PayloadPattern::Zeros,
            PayloadPattern::Repeated,
            PayloadPattern::StructuredText,
        ] {
            let first = payload_for(pattern, 17, 4096, 0x1234_5678);
            let second = payload_for(pattern, 17, 4096, 0x1234_5678);
            assert_eq!(first.len(), 4096);
            assert_eq!(first, second);
            assert!(first.is_ascii());
        }
    }

    #[test]
    fn high_entropy_ascii_uses_broad_alphabet() {
        let value = payload_for(PayloadPattern::RandomAscii, 9, 4096, 0x5eed_2026);
        let mut seen = [false; 128];
        for byte in value.bytes() {
            seen[usize::from(byte)] = true;
        }
        assert!(seen.into_iter().filter(|present| *present).count() > 80);
    }

    #[test]
    fn structured_and_low_entropy_shapes_are_distinct() {
        let structured = payload_for(PayloadPattern::StructuredText, 1, 512, 2);
        let repeated = payload_for(PayloadPattern::Repeated, 1, 512, 2);
        let zeros = payload_for(PayloadPattern::Zeros, 1, 512, 2);
        assert!(structured.contains("\"schema\":\"telemetry-v1\""));
        let repeated_bytes = repeated.as_bytes();
        assert!(
            repeated_bytes
                .chunks_exact(8)
                .all(|chunk| chunk == &repeated_bytes[..8])
        );
        assert_ne!(repeated, zeros);
        assert!(zeros.bytes().all(|byte| byte == b'0'));
    }
}
