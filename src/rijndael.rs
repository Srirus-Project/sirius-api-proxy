//! Sirius legacy Rijndael-256 (256-bit block AND key), not AES-256.
//! Decryption only. Used for Master files,
//! not as a general-purpose cryptographic API.
use crate::master::MasterError;

fn mul(mut a: u8, mut b: u8) -> u8 {
    let mut result = 0;
    for _ in 0..8 {
        if b & 1 != 0 {
            result ^= a;
        }
        a = (a << 1) ^ if a & 128 != 0 { 0x1b } else { 0 };
        b >>= 1;
    }
    result
}
fn boxes() -> ([u8; 256], [u8; 256]) {
    let mut sbox = [0; 256];
    let mut inverse = [0; 256];
    for (i, value) in sbox.iter_mut().enumerate() {
        let mut x = 1;
        // Multiplicative inverse x^254; zero maps to zero.
        for _ in 0..254 {
            x = mul(x, i as u8);
        }
        *value =
            x ^ x.rotate_left(1) ^ x.rotate_left(2) ^ x.rotate_left(3) ^ x.rotate_left(4) ^ 0x63;
        inverse[*value as usize] = i as u8;
    }
    (sbox, inverse)
}
pub(crate) struct Rijndael256 {
    words: [[u8; 4]; 120],
    inverse: [u8; 256],
}
impl Rijndael256 {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        let (sbox, inverse) = boxes();
        let mut words = [[0; 4]; 120];
        for (w, bytes) in words.iter_mut().zip(key.chunks_exact(4)) {
            w.copy_from_slice(bytes);
        }
        let mut rcon = 1;
        for i in 8..120 {
            let mut t = words[i - 1];
            if i % 8 == 0 {
                t.rotate_left(1);
                t = t.map(|b| sbox[b as usize]);
                t[0] ^= rcon;
                rcon = mul(rcon, 2);
            } else if i % 8 == 4 {
                t = t.map(|b| sbox[b as usize]);
            }
            for (j, b) in t.iter().enumerate() {
                words[i][j] = words[i - 8][j] ^ b;
            }
        }
        Self { words, inverse }
    }
    fn add_key(&self, state: &mut [u8; 32], round: usize) {
        for (i, b) in state.iter_mut().enumerate() {
            *b ^= self.words[round * 8 + i / 4][i % 4];
        }
    }
    fn block(&self, input: &[u8]) -> [u8; 32] {
        let mut state: [u8; 32] = input.try_into().expect("caller passes a full block");
        self.add_key(&mut state, 14);
        for round in (0..14).rev() {
            let previous = state;
            for column in 0..8 {
                for (row, shift) in [0, 1, 3, 4].iter().enumerate() {
                    state[column * 4 + row] =
                        self.inverse[previous[((column + 8 - shift) % 8) * 4 + row] as usize];
                }
            }
            self.add_key(&mut state, round);
            if round != 0 {
                for column in state.chunks_exact_mut(4) {
                    let a: [u8; 4] = column.try_into().unwrap();
                    for row in 0..4 {
                        column[row] = mul(a[row], 14)
                            ^ mul(a[(row + 1) % 4], 11)
                            ^ mul(a[(row + 2) % 4], 13)
                            ^ mul(a[(row + 3) % 4], 9);
                    }
                }
            }
        }
        state
    }
    pub(crate) fn decrypt(&self, encrypted: &[u8], iv: &[u8; 32]) -> Result<Vec<u8>, MasterError> {
        if encrypted.is_empty() || !encrypted.len().is_multiple_of(32) {
            return Err(MasterError::Cipher);
        }
        let mut previous = iv.as_slice();
        let mut output = Vec::with_capacity(encrypted.len());
        for block in encrypted.chunks_exact(32) {
            let plain = self.block(block);
            output.extend(plain.iter().zip(previous).map(|(a, b)| a ^ b));
            previous = block;
        }
        let padding = *output.last().ok_or(MasterError::Cipher)? as usize;
        if !(1..=32).contains(&padding)
            || !output[output.len() - padding..]
                .iter()
                .all(|b| *b as usize == padding)
        {
            return Err(MasterError::Cipher);
        }
        output.truncate(output.len() - padding);
        Ok(output)
    }
}
