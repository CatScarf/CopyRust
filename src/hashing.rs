use crate::HashAlgorithm;
use std::hash::Hasher;

#[derive(PartialEq, Eq)]
pub(crate) enum Digest {
    Short(u64),
    Wide(blake3::Hash),
}

pub(crate) enum ContentHash {
    Xxh3(Box<twox_hash::XxHash3_64>),
    Xxh64(twox_hash::XxHash64),
    Blake3(Box<blake3::Hasher>),
}

impl ContentHash {
    /// Construct a deterministic seed-zero hasher with portable SIMD dispatch.
    pub(crate) fn new(algorithm: HashAlgorithm) -> Self {
        match algorithm {
            HashAlgorithm::Xxh3 => Self::Xxh3(Box::default()),
            HashAlgorithm::Xxh64 => Self::Xxh64(twox_hash::XxHash64::with_seed(0)),
            HashAlgorithm::Blake3 => Self::Blake3(Box::default()),
        }
    }

    /// Consume the complete byte stream without sampling or metadata shortcuts.
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Xxh3(hash) => hash.write(bytes),
            Self::Xxh64(hash) => hash.write(bytes),
            Self::Blake3(hash) => {
                hash.update(bytes);
            }
        }
    }

    /// Return a digest with its native output width.
    pub(crate) fn finish(&self) -> Digest {
        match self {
            Self::Xxh3(hash) => Digest::Short(hash.finish()),
            Self::Xxh64(hash) => Digest::Short(hash.finish()),
            Self::Blake3(hash) => Digest::Wide(hash.finalize()),
        }
    }
}
