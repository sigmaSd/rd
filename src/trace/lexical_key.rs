use crate::util::u8_slice;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct LexicalKey128 {
    key1: [u8; 8],
    key2: [u8; 8],
}

impl AsRef<[u8]> for LexicalKey128 {
    fn as_ref(&self) -> &[u8] {
        u8_slice(self)
    }
}

impl LexicalKey128 {
    pub fn new(key1: u64, key2: u64) -> LexicalKey128 {
        LexicalKey128 {
            key1: key1.to_be_bytes(),
            key2: key2.to_be_bytes(),
        }
    }

    pub fn key1(&self) -> u64 {
        u64::from_be_bytes(self.key1)
    }

    pub fn key2(&self) -> u64 {
        u64::from_be_bytes(self.key2)
    }
}
