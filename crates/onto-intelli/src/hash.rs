//! Stable 128-bit hashing for cache keys that outlive the process
//! (std's `DefaultHasher` is not stable across Rust releases).

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub fn stable_hash(parts: &[&str]) -> String {
    let mut a = FNV_OFFSET;
    let mut b = FNV_OFFSET ^ 0x9e37_79b9_7f4a_7c15;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0x1f)) {
            a = (a ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
            b = (b ^ u64::from(byte)).wrapping_mul(FNV_PRIME).rotate_left(29);
        }
    }
    format!("{a:016x}{b:016x}")
}

/// Lowercase, collapse whitespace, strip trailing punctuation: questions that
/// differ only cosmetically share cache entries.
pub fn normalize_question(q: &str) -> String {
    let collapsed = q.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    collapsed.trim_end_matches(['?', '!', '.', ' ']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_separator_aware() {
        assert_eq!(stable_hash(&["a", "b"]), stable_hash(&["a", "b"]));
        assert_ne!(stable_hash(&["ab", ""]), stable_hash(&["a", "b"]));
        assert_eq!(normalize_question("  Who owns   Alpha?? "), "who owns alpha");
    }
}
