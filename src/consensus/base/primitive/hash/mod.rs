extern crate blake2_rfc;
extern crate libargon2_sys;
extern crate sha2;

use std::str;
use hex::{FromHex,FromHexError};
use self::blake2_rfc::blake2b::Blake2b;
use self::libargon2_sys::argon2d_hash;
use self::sha2::{Sha256,Digest};

pub trait Hasher {
    type Output;

    fn finish(self) -> Self::Output;
    fn write(&mut self, bytes: &[u8]) -> &mut Self;
    fn digest(self, bytes: &[u8]) -> Self::Output;
}

pub trait Hash {
    fn hash<H>(&self, state: &mut H) where H: Hasher;
}

const BLAKE2B_LENGTH : usize = 32;
create_typed_array!(Blake2bHash, u8, BLAKE2B_LENGTH);
add_hex_io_fns!(Blake2bHash, BLAKE2B_LENGTH);
pub struct Blake2bHasher(Blake2b);

impl Blake2bHasher {
    pub fn new() -> Self {
        return Blake2bHasher(Blake2b::new(BLAKE2B_LENGTH));
    }
}

impl Default for Blake2bHasher {
    fn default() -> Self {
        return Blake2bHasher::new();
    }
}

impl Hasher for Blake2bHasher {
    type Output = Blake2bHash;

    fn finish(self) -> Blake2bHash {
        let result = self.0.finalize();
        return Blake2bHash::from(result.as_bytes());
    }

    fn write(&mut self, bytes: &[u8]) -> &mut Blake2bHasher {
        self.0.update(bytes);
        return self;
    }

    fn digest(mut self, bytes: &[u8]) -> Blake2bHash {
        self.write(bytes);
        return self.finish();
    }
}

const ARGON2D_LENGTH : usize = 32;
const NIMIQ_ARGON2_SALT: &'static str = "nimiqrocks!";
const DEFAULT_ARGON2_COST : u32 = 512;
create_typed_array!(Argon2dHash, u8, ARGON2D_LENGTH);
add_hex_io_fns!(Argon2dHash, ARGON2D_LENGTH);
pub struct Argon2dHasher {
    buf: Vec<u8>,
    passes: u32,
    lanes: u32,
    kib: u32
}

impl Argon2dHasher {
    pub fn new(passes: u32, lanes: u32, kib: u32) -> Self {
        return Argon2dHasher { buf: vec![], passes, lanes, kib };
    }

    fn hash(&self, bytes: &[u8], salt: &[u8]) -> Argon2dHash {
        let mut out = [0u8; ARGON2D_LENGTH];
        argon2d_hash(self.passes, self.kib, self.lanes,bytes, salt, &mut out, 0);
        return Argon2dHash::from(out);
    }
}

impl Default for Argon2dHasher {
    fn default() -> Self {
        return Argon2dHasher::new(1, 1, DEFAULT_ARGON2_COST);
    }
}

impl Hasher for Argon2dHasher {
    type Output = Argon2dHash;

    fn finish(self) -> Argon2dHash {
        return self.hash(self.buf.as_slice(), NIMIQ_ARGON2_SALT.as_bytes());
    }

    fn write(&mut self, bytes: &[u8]) -> &mut Argon2dHasher {
        self.buf.extend(bytes);
        return self;
    }

    fn digest(self, bytes: &[u8]) -> Argon2dHash {
        return self.hash(bytes, NIMIQ_ARGON2_SALT.as_bytes());
    }
}

const SHA256_LENGTH : usize = 32;
create_typed_array!(Sha256Hash, u8, SHA256_LENGTH);
add_hex_io_fns!(Sha256Hash, SHA256_LENGTH);
pub struct Sha256Hasher(Sha256);

impl Sha256Hasher {
    pub fn new() -> Self {
        return Sha256Hasher(Sha256::default());
    }
}

impl Default for Sha256Hasher {
    fn default() -> Self {
        return Sha256Hasher::new();
    }
}

impl Hasher for Sha256Hasher {
    type Output = Sha256Hash;

    fn finish(self) -> Sha256Hash {
        let result = self.0.result();
        return Sha256Hash::from(result.as_slice());
    }

    fn write(&mut self, bytes: &[u8]) -> &mut Sha256Hasher {
        self.0.input(bytes);
        return self;
    }

    fn digest(mut self, bytes: &[u8]) -> Sha256Hash {
        self.write(bytes);
        return self.finish();
    }
}
