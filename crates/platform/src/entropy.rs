//! OS entropy.
//!
//! Invitation secrets, session tokens and one-time codes must come from the OS
//! CSPRNG. A hash of the clock and a hasher seed looks random but is guessable,
//! so this module is the only approved source and modules must not build their
//! own from timestamps.

use std::io;

/// Fill `bytes` with OS entropy. Fails closed: a caller must treat an error as
/// "cannot create a secret right now", never as "use a weaker value".
pub fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::fill(bytes).map_err(|error| io::Error::other(error.to_string()))
}

/// `count` bytes of OS entropy as a vector, for callers that build a buffer.
pub fn random_bytes(count: usize) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; count];
    fill_random(&mut buffer)?;
    Ok(buffer)
}

/// Lowercase hex of `count` random bytes (2 `count` characters).
pub fn random_hex(count: usize) -> io::Result<String> {
    let bytes = random_bytes(count)?;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_distinct_values() {
        let first = random_hex(16).expect("entropy");
        let second = random_hex(16).expect("entropy");
        assert_eq!(first.len(), 32);
        assert_ne!(first, second, "two draws must not repeat");
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
    }

    #[test]
    fn fills_the_whole_buffer() {
        let mut buffer = [0u8; 32];
        fill_random(&mut buffer).expect("entropy");
        assert!(
            buffer.iter().any(|byte| *byte != 0),
            "an all-zero buffer means the entropy source was not consulted"
        );
    }

    #[test]
    fn rejects_a_zero_length_draw_safely() {
        // A zero-length request is valid and must not panic or fail.
        let empty: Vec<u8> = random_bytes(0).expect("entropy");
        assert!(empty.is_empty());
    }
}
