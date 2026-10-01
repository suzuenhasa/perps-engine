//! CRC32C, the checksum on every journal record and segment header (`docs/PIPELINE.md`
//! 11.4; `docs/DECISIONS.md` D-025). Our own, in safe Rust: no new crate.
//!
//! **Contract.** The Castagnoli polynomial, reflected (`0x82F63B78`), initial value
//! `0xFFFFFFFF`, final XOR `0xFFFFFFFF`: the CRC that ext4, iSCSI, LevelDB and RocksDB use.
//! Check value: `crc32c(b"123456789") == 0xE3069283`, and `crc32c(b"") == 0`. [`Crc32c`]
//! computes one CRC over several pieces, which a journal record needs (its CRC covers bytes
//! 0..4 and then 8..len, every byte but the CRC field itself).
//!
//! **How: slicing by 8.** A table-driven CRC handles one byte per step: XOR the byte into
//! the low byte of the running CRC, shift the CRC right by 8, and XOR in the table entry for
//! the byte that fell out. Each step needs the previous step's result, so it runs at about
//! 6 to 7 cycles a byte. Slicing by 8 uses eight tables: `TABLES[k][b]` is the effect of
//! byte `b` followed by `k` zero bytes. So 8 bytes can be handled in one step: XOR the
//! running CRC into the next 8 bytes (read as a little-endian `u64`), then XOR together
//! eight independent lookups, one per byte, byte 0 in table 7 (it has 7 bytes still to pass
//! through) and byte 7 in table 0. The lookups don't depend on each other, so the CPU does
//! them in parallel: typically 1 to 1.5 cycles a byte. The last `len % 8` bytes go one at a
//! time through table 0. (x86's CRC32C instruction would be faster still, but it needs
//! `unsafe` intrinsics; not worth it.)
//!
//! **Tables.** 8 × 256 `u32` = 8 KiB, built at compile time by a `const fn`.
//!
//! **Reference.** [`crc32c_bytewise`], one byte per step through table 0, is kept as the
//! reference that the tests compare slicing by 8 with on random inputs, and for the
//! `pipeline_parts` benchmark, which measures both.
//!
//! **What it protects against:** torn writes, and any corruption that doesn't happen to
//! keep the CRC valid (a random corruption passes with probability 2^-32). It is not a
//! security measure.

/// The Castagnoli polynomial, bit-reflected.
const POLYNOMIAL: u32 = 0x82F6_3B78;

/// `TABLES[k][b]`: the CRC contribution of byte `b` followed by `k` zero bytes.
static TABLES: [[u32; 256]; 8] = build_tables();

const fn build_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    // Table 0: the classic one-byte table, eight shift-and-maybe-XOR steps per entry.
    let mut b = 0;
    while b < 256 {
        let mut crc = b as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ POLYNOMIAL } else { crc >> 1 };
            bit += 1;
        }
        tables[0][b] = crc;
        b += 1;
    }
    // Table k: table k - 1's entry pushed through one more zero byte.
    let mut k = 1;
    while k < 8 {
        let mut b = 0;
        while b < 256 {
            let previous = tables[k - 1][b];
            tables[k][b] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            b += 1;
        }
        k += 1;
    }
    tables
}

/// A CRC32C being computed over one or more pieces of input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crc32c {
    /// The running register: starts at `0xFFFFFFFF`; [`Crc32c::finish`] inverts it.
    state: u32,
}

impl Default for Crc32c {
    fn default() -> Self {
        Crc32c::new()
    }
}

impl Crc32c {
    pub const fn new() -> Self {
        Crc32c { state: 0xFFFF_FFFF }
    }

    /// Adds `bytes` to the input, by slicing by 8.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.state;
        let (chunks, rest) = bytes.as_chunks::<8>();
        for chunk in chunks {
            let word = u64::from_le_bytes(*chunk) ^ u64::from(crc);
            let byte = |i: u32| ((word >> (8 * i)) & 0xFF) as usize;
            crc = TABLES[7][byte(0)]
                ^ TABLES[6][byte(1)]
                ^ TABLES[5][byte(2)]
                ^ TABLES[4][byte(3)]
                ^ TABLES[3][byte(4)]
                ^ TABLES[2][byte(5)]
                ^ TABLES[1][byte(6)]
                ^ TABLES[0][byte(7)];
        }
        for &b in rest {
            crc = step(crc, b);
        }
        self.state = crc;
    }

    /// The CRC of everything added so far.
    pub const fn finish(self) -> u32 {
        !self.state
    }
}

/// One byte through table 0.
fn step(crc: u32, byte: u8) -> u32 {
    (crc >> 8) ^ TABLES[0][((crc ^ u32::from(byte)) & 0xFF) as usize]
}

/// The CRC32C of `bytes`, by slicing by 8.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(bytes);
    crc.finish()
}

/// The CRC32C of `bytes`, one byte at a time: the reference for the tests and the
/// benchmark (module docs). Same result as [`crc32c`], several times slower.
pub fn crc32c_bytewise(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(0xFFFF_FFFF, |crc, &b| step(crc, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::spec_examples;

    /// The CRC computed one bit at a time, straight from the definition, with no table:
    /// an independent check of table 0.
    fn crc32c_bitwise(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ POLYNOMIAL } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn the_standard_check_value_and_the_empty_input() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_bytewise(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_bitwise(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c_bytewise(b""), 0);
    }

    #[test]
    fn the_worked_journal_records_have_the_specs_crcs() {
        // PIPELINE.md 11.3: the CRC covers bytes 0..4, then 8..len, and is stored,
        // little-endian, in bytes 4..8.
        for (hex, expected) in
            [(spec_examples::RECORD_1, 0xE5C5_CC1D), (spec_examples::RECORD_2, 0xFF8E_EBC8)]
        {
            let bytes = spec_examples::bytes(hex);
            let mut crc = Crc32c::new();
            crc.update(&bytes[0..4]);
            crc.update(&bytes[8..]);
            assert_eq!(crc.finish(), expected);
            assert_eq!(bytes[4..8], expected.to_le_bytes());
        }
    }

    #[test]
    fn slicing_by_8_equals_byte_at_a_time_on_random_inputs_of_every_length() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for len in 0..=300 {
            for _ in 0..8 {
                let bytes: Vec<u8> = (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state as u8
                    })
                    .collect();
                let expected = crc32c_bytewise(&bytes);
                assert_eq!(crc32c(&bytes), expected, "length {len}");
                assert_eq!(crc32c_bitwise(&bytes), expected, "length {len}");
            }
        }
    }

    #[test]
    fn pieces_give_the_same_crc_as_the_whole() {
        let bytes: Vec<u8> = (0..=255).collect();
        for split in [0, 1, 7, 8, 9, 100, 255, 256] {
            let mut crc = Crc32c::default();
            crc.update(&bytes[..split]);
            crc.update(&bytes[split..]);
            assert_eq!(crc.finish(), crc32c(&bytes), "split at {split}");
        }
    }
}
