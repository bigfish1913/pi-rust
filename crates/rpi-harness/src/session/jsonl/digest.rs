//! SHA-256 for JSONL source fingerprints and cold-payload integrity checks.
//!
//! `ring` is already used by the workspace's reqwest/rustls stack. Its backend
//! selects supported CPU acceleration without changing the algorithm or adding
//! sha2's optional assembly feature to other targets. Keep reads, lengths and
//! verification order at the call sites unchanged; this is not a trust cache.

#[derive(Clone)]
pub(super) struct Sha256(ring::digest::Context);

impl Sha256 {
    pub(super) fn new() -> Self {
        Self(ring::digest::Context::new(&ring::digest::SHA256))
    }

    pub(super) fn update(&mut self, bytes: impl AsRef<[u8]>) {
        self.0.update(bytes.as_ref());
    }

    pub(super) fn finalize(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        bytes.copy_from_slice(self.0.finish().as_ref());
        bytes
    }

    pub(super) fn digest(bytes: impl AsRef<[u8]>) -> [u8; 32] {
        let mut hash = Self::new();
        hash.update(bytes);
        hash.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::Sha256;
    use sha2::Digest;

    #[test]
    fn standard_sha256_vectors() {
        for (input, expected) in [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".as_slice(),
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            let actual: String = Sha256::digest(input)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(actual, expected);
        }
        let actual: String = Sha256::digest(vec![b'a'; 1_000_000])
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            actual,
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn matches_sha2_across_padding_and_reader_boundaries() {
        const CHUNK: usize = 1024 * 1024;
        for length in [
            0,
            1,
            55,
            56,
            63,
            64,
            65,
            127,
            128,
            129,
            1023,
            1024,
            1025,
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
        ] {
            let input: Vec<u8> = (0..length)
                .map(|i| ((i * 73 + i / 17) % 256) as u8)
                .collect();
            let expected: [u8; 32] = sha2::Sha256::digest(&input).into();
            assert_eq!(Sha256::digest(&input), expected, "length {length}");
            let chunks: &[usize] = if length < 4096 {
                &[1, 7, 55, 64, 65, 4096]
            } else {
                &[4093, CHUNK]
            };
            for &chunk_size in chunks {
                let mut hash = Sha256::new();
                for chunk in input.chunks(chunk_size) {
                    hash.update(chunk);
                }
                assert_eq!(
                    hash.finalize(),
                    expected,
                    "length {length}, chunk {chunk_size}"
                );
            }
            // Streaming repair captures a digest of the validated prefix, then
            // continues hashing a repaired tail using the original context.
            let (prefix, tail) = input.split_at(length / 2);
            let mut hash = Sha256::new();
            hash.update(prefix);
            let prefix_expected: [u8; 32] = sha2::Sha256::digest(prefix).into();
            assert_eq!(hash.clone().finalize(), prefix_expected);
            hash.update(tail);
            assert_eq!(hash.finalize(), expected);
        }
    }
}
