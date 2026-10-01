//! keccak-256, Ethereum's hash (`docs/PIPELINE.md` 5.8; `docs/DECISIONS.md` D-033). Our own,
//! in safe Rust: no new crate (the owner's choice).
//!
//! **What it is.** Keccak is a *sponge*: a state of 1600 bits, held as 25 lanes of 64 bits,
//! and a permutation, Keccak-f[1600], that mixes the whole state in 24 rounds. To hash, the
//! input is cut into blocks of [`RATE`] = 136 bytes. Each block is XORed into the first 136
//! bytes of the state (17 lanes, each read little-endian), and the state is permuted. The
//! other 64 bytes, the "capacity", are never written directly; they are what gives the hash
//! its strength. The output is the first 32 bytes of the state after the last block.
//!
//! **Padding, and why this is not SHA3-256.** The last block is padded: the byte `0x01`
//! right after the data, `0x80` in the block's last byte, zeros between (both in one byte,
//! `0x81`, when exactly one byte is left). An input of whole blocks gets a last block of
//! padding alone. Ethereum adopted Keccak before NIST standardised it as SHA-3 (FIPS 202),
//! and NIST changed only that first padding byte, to `0x06`. Same permutation, same rate,
//! different hashes: keccak-256 of nothing is `c5d24601…`, SHA3-256 of nothing `a7ffc6f8…`.
//! [`keccak256`] uses `0x01`. The tests also run the same sponge with `0x06`, to check it
//! against NIST's SHA3-256 known answers at the edge of a block (135 and 136 bytes), which
//! the keccak-256 known answers we have don't reach.
//!
//! **The permutation**, one round, on the lanes `A[x, y]` (`x` and `y` in 0..5, and every
//! index taken mod 5):
//! - θ (theta): the parity of each column, `C[x] = A[x, 0] ^ A[x, 1] ^ … ^ A[x, 4]`; then
//!   every lane `A[x, y] ^= C[x − 1] ^ rot(C[x + 1], 1)`.
//! - ρ (rho) and π (pi): each lane is rotated left by its own fixed offset and moved:
//!   `B[y, 2x + 3y] = rot(A[x, y], RHO[x, y])`.
//! - χ (chi), the only step that is not linear, row by row:
//!   `A[x, y] = B[x, y] ^ (!B[x + 1, y] & B[x + 2, y])`.
//! - ι (iota): `A[0, 0] ^= RC[round]`, a constant per round, so that the rounds differ.
//!
//! Lane `A[x, y]` is `state[x + 5y]`. This is the layout of the Keccak team's reference,
//! `KeccakF-1600-IntermediateValues.txt` in their XKCP repository: the round constants and
//! rotation offsets below are copied from it, and its two worked permutations are tests
//! here. Both tables also follow from short rules (FIPS 202; at each table), which a test
//! applies to derive them again.
//!
//! **Allocation.** None: the state (200 bytes), a copy of the last block (136 bytes) and
//! one round's scratch lanes are on the stack.
//!
//! **Complexity.** One permutation per whole block, plus one for the last: up to 135 bytes
//! take one permutation, 136 to 271 two. Every hash of the EIP-712 scheme fits in one block
//! (at most 135 bytes), so each takes one permutation; only the tests' 160-byte domains
//! take two.

/// Bytes absorbed per permutation: 1600 bits minus a capacity of 512, twice the output.
pub const RATE: usize = 136;

/// Rounds of Keccak-f[1600].
const ROUNDS: usize = 24;

/// ι's round constants, `RC[0]` to `RC[23]` (XKCP, "The round constants"). The rule
/// (FIPS 202, Algorithm 5): an 8-bit shift register, with the polynomial
/// `x^8 + x^6 + x^5 + x^4 + 1`, gives one bit per step, and round `i` takes the bits of
/// steps `7i` to `7i + 6` and puts them at bit positions 0, 1, 3, 7, 15, 31 and 63
/// (`2^j − 1`); every other bit is 0.
const ROUND_CONSTANTS: [u64; ROUNDS] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808A,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808B,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008A,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000A,
    0x0000_0000_8000_808B,
    0x8000_0000_0000_008B,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800A,
    0x8000_0000_8000_000A,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

/// ρ's rotation offsets, `RHO[x + 5y]` (XKCP, "The rho offsets"). The rule (FIPS 202,
/// Algorithm 2): `A[0, 0]` is not rotated; the other 24 lanes are visited once each by the
/// walk that starts at `(1, 0)` and steps `(x, y) → (y, 2x + 3y)`, and the lane reached at
/// step `t` (0 to 23) is rotated by the triangular number `(t + 1)(t + 2) / 2`, mod 64.
const RHO: [u32; 25] = [
    0, 1, 62, 28, 27, //
    36, 44, 6, 55, 20, //
    3, 10, 43, 25, 39, //
    41, 45, 15, 21, 8, //
    18, 2, 61, 56, 14,
];

/// The first padding byte of Keccak as Ethereum uses it (module docs). SHA3-256's is `0x06`.
const KECCAK_PADDING: u8 = 0x01;

/// Ethereum's keccak-256 of `data`, of any length (module docs).
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    sponge(data, KECCAK_PADDING)
}

/// The 256-bit sponge over `data`, with `padding` as the first padding byte (module docs).
fn sponge(data: &[u8], padding: u8) -> [u8; 32] {
    let mut state = [0u64; 25];
    let (blocks, rest) = data.as_chunks::<RATE>();
    for block in blocks {
        absorb(&mut state, block);
    }
    // The last block: what is left of the data (0 to 135 bytes), then the padding. `rest`
    // is always shorter than a block, so there is room for the padding's first byte.
    let mut last = [0u8; RATE];
    last[..rest.len()].copy_from_slice(rest);
    last[rest.len()] ^= padding;
    last[RATE - 1] ^= 0x80;
    absorb(&mut state, &last);
    // The output: the first 32 bytes of the state, lanes 0 to 3, little-endian.
    let mut output = [0u8; 32];
    for (bytes, lane) in output.as_chunks_mut::<8>().0.iter_mut().zip(state) {
        *bytes = lane.to_le_bytes();
    }
    output
}

/// XORs one block into the first 17 lanes, then permutes the state.
fn absorb(state: &mut [u64; 25], block: &[u8; RATE]) {
    for (lane, bytes) in state.iter_mut().zip(block.as_chunks::<8>().0) {
        *lane ^= u64::from_le_bytes(*bytes);
    }
    keccak_f(state);
}

/// Keccak-f[1600]: 24 rounds of θ, ρ and π, χ, and ι on the 25 lanes, where `A[x, y]` is
/// `a[x + 5 * y]` (module docs). The loops run over `x` and `y` themselves, 0 to 4 each, so
/// the compiler unrolls them and works out every lane's index while compiling. (Walking the
/// 25 lanes with one index and dividing it by 5 made the permutation about 7 times slower;
/// `docs/PIPELINE.md` 22.)
fn keccak_f(a: &mut [u64; 25]) {
    for round_constant in ROUND_CONSTANTS {
        // θ: the parity of each column, then each lane XORed with the parities of the
        // columns on either side (the right one rotated by one bit).
        let c: [u64; 5] = std::array::from_fn(|x| a[x] ^ a[x + 5] ^ a[x + 10] ^ a[x + 15] ^ a[x + 20]);
        for x in 0..5 {
            let d = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
            for y in 0..5 {
                a[x + 5 * y] ^= d;
            }
        }
        // ρ and π: lane A[x, y], rotated by its offset, becomes B[y, 2x + 3y].
        let mut b = [0u64; 25];
        for y in 0..5 {
            for x in 0..5 {
                b[y + 5 * ((2 * x + 3 * y) % 5)] = a[x + 5 * y].rotate_left(RHO[x + 5 * y]);
            }
        }
        // χ: each lane combined with the next two in its row.
        for y in 0..5 {
            for x in 0..5 {
                a[x + 5 * y] = b[x + 5 * y] ^ (!b[(x + 1) % 5 + 5 * y] & b[(x + 2) % 5 + 5 * y]);
            }
        }
        // ι
        a[0] ^= round_constant;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::bytes;

    /// FIPS 202 SHA3-256: the same sponge with NIST's padding byte (module docs).
    fn sha3_256(data: &[u8]) -> [u8; 32] {
        sponge(data, 0x06)
    }

    #[test]
    fn keccak256_of_nothing_and_of_abc() {
        // go-ethereum: `EmptyCodeHash = crypto.Keccak256Hash(nil) // c5d24601…`
        // (core/types/hashes.go), and `TestKeccak256Hash` hashes "abc" (crypto/crypto_test.go;
        // also golang.org/x/crypto's sha3 `TestKeccak` for `NewLegacyKeccak256`).
        assert_eq!(
            keccak256(b"")[..],
            bytes("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
        );
        assert_eq!(
            keccak256(b"abc")[..],
            bytes("4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45")
        );
    }

    #[test]
    fn the_permutation_matches_the_keccak_teams_worked_examples() {
        // XKCP, tests/TestVectors/KeccakF-1600-IntermediateValues.txt: "Example with the
        // all-zero input", then "Example taking the previous output as input". The lanes
        // after round 23 ("After iota"), in the file's order, A[0, 0] first.
        let first: [u64; 25] = [
            0xF125_8F79_40E1_DDE7,
            0x84D5_CCF9_33C0_478A,
            0xD598_261E_A65A_A9EE,
            0xBD15_4730_6F80_494D,
            0x8B28_4E05_6253_D057,
            0xFF97_A42D_7F8E_6FD4,
            0x90FE_E5A0_A446_47C4,
            0x8C5B_DA0C_D619_2E76,
            0xAD30_A6F7_1B19_059C,
            0x3093_5AB7_D08F_FC64,
            0xEB5A_A93F_2317_D635,
            0xA9A6_E626_0D71_2103,
            0x81A5_7C16_DBCF_555F,
            0x43B8_31CD_0347_C826,
            0x01F2_2F1A_11A5_569F,
            0x05E5_635A_21D9_AE61,
            0x64BE_FEF2_8CC9_70F2,
            0x6136_7095_7BC4_6611,
            0xB87C_5A55_4FD0_0ECB,
            0x8C3E_E88A_1CCF_32C8,
            0x940C_7922_AE3A_2614,
            0x1841_F924_A2C5_09E4,
            0x16F5_3526_E704_65C2,
            0x75F6_44E9_7F30_A13B,
            0xEAF1_FF7B_5CEC_A249,
        ];
        let second: [u64; 25] = [
            0x2D5C_954D_F96E_CB3C,
            0x6A33_2CD0_7057_B56D,
            0x093D_8D12_70D7_6B6C,
            0x8A20_D9B2_5569_D094,
            0x4F9C_4F99_E5E7_F156,
            0xF957_B9A2_DA65_FB38,
            0x8577_3DAE_1275_AF0D,
            0xFAF4_F247_C3D8_10F7,
            0x1F1B_9EE6_F79A_8759,
            0xE4FE_CC0F_EE98_B425,
            0x68CE_61B6_B9CE_68A1,
            0xDEEA_66C4_BA8F_974F,
            0x33C4_3D83_6EAF_B1F5,
            0xE006_5404_2719_DBD9,
            0x7CF8_A9F0_0983_1265,
            0xFD54_49A6_BF17_4743,
            0x97DD_AD33_D899_4B40,
            0x48EA_D5FC_5D0B_E774,
            0xE3B8_C8EE_55B7_B03C,
            0x91A0_226E_649E_42E9,
            0x900E_3129_E7BA_DD7B,
            0x202A_9EC5_FAA3_CCE8,
            0x5B34_0246_4E1C_3DB6,
            0x609F_4E62_A44C_1059,
            0x20D0_6CD2_6A8F_BF5C,
        ];
        let mut state = [0u64; 25];
        keccak_f(&mut state);
        assert_eq!(state, first);
        keccak_f(&mut state);
        assert_eq!(state, second);
    }

    #[test]
    fn the_tables_follow_their_rules() {
        // FIPS 202, Algorithms 2 and 5, as XKCP's reference implementation computes them
        // (lib/low/KeccakP-1600/ref-64bits/KeccakP-1600-reference.c:
        // `KeccakP1600_InitializeRhoOffsets`, `KeccakP1600_InitializeRoundConstants`).
        let mut rho = [0u32; 25];
        let (mut x, mut y) = (1, 0);
        for t in 0..24 {
            rho[x + 5 * y] = ((t + 1) * (t + 2) / 2 % 64) as u32;
            (x, y) = (y, (2 * x + 3 * y) % 5);
        }
        assert_eq!(rho, RHO);
        // The shift register: its output is its lowest bit. Each step shifts it left by one,
        // and when a bit falls off the top (x^8), XORs in the polynomial's other terms,
        // x^6 + x^5 + x^4 + 1, which are the bits 0x71.
        let mut register: u8 = 1;
        let mut next_bit = || {
            let bit = u64::from(register & 1);
            register = if register & 0x80 != 0 { (register << 1) ^ 0x71 } else { register << 1 };
            bit
        };
        let round_constants: [u64; ROUNDS] =
            std::array::from_fn(|_| (0..7).fold(0, |constant, j| constant | (next_bit() << ((1 << j) - 1))));
        assert_eq!(round_constants, ROUND_CONSTANTS);
    }

    #[test]
    fn the_sponge_matches_nists_sha3_256_answers_at_the_edge_of_a_block() {
        // XKCP, tests/TestVectors/ShortMsgKAT_SHA3-256.txt ("Keccak(input|01)[r=1088, c=512]
        // truncated to 256 bits, or SHA3-256 as in FIPS 202"): Len = 0, 8, 1080 and 1088
        // bits. 135 bytes leave one byte for both padding bytes (0x06 ^ 0x80); 136 bytes are
        // a whole block, so the padding takes a block of its own.
        assert_eq!(
            sha3_256(b"")[..],
            bytes("a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a")
        );
        assert_eq!(
            sha3_256(&[0xCC])[..],
            bytes("677035391cd3701293d385f037ba32796252bb7ce180b00b582dd9b20aaad7f0")
        );
        let bytes_135 = bytes(
            "b771d5cef5d1a41a93d15643d7181d2a2ef0a8e84d91812f20ed21f147bef732
             bf3a60ef4067c3734b85bc8cd471780f10dc9e8291b58339a677b960218f71e7
             93f2797aea349406512829065d37bb55ea796fa4f56fd8896b49b2cd19b43215
             ad967c712b24e5032d065232e02c127409d2ed4146b9d75d763d52db98d949d3
             b0fed6a8052fbb",
        );
        assert_eq!(bytes_135.len(), 135);
        assert_eq!(
            sha3_256(&bytes_135)[..],
            bytes("a19eee92bb2097b64e823d597798aa18be9b7c736b8059abfd6779ac35ac81b5")
        );
        let bytes_136 = bytes(
            "b32d95b0b9aad2a8816de6d06d1f86008505bd8c14124f6e9a163b5a2ade55f8
             35d0ec3880ef50700d3b25e42cc0af050ccd1be5e555b23087e04d7bf9813622
             780c7313a1954f8740b6ee2d3f71f768dd417f520482bd3a08d4f222b4ee9dbd
             015447b33507dd50f3ab4247c5de9a8abd62a8decea01e3b87c8b927f5b08beb
             37674c6f8e380c04",
        );
        assert_eq!(bytes_136.len(), RATE);
        assert_eq!(
            sha3_256(&bytes_136)[..],
            bytes("df673f4105379ff6b755eeab20ceb0dc77b5286364fe16c59cc8a907aff07732")
        );
    }

    #[test]
    fn keccak256_over_two_blocks_the_eip712_mail_domain_separator() {
        // The EIP-712 specification's example (ethereum/EIPs, assets/eip-712/Example.js):
        // `structHash('EIP712Domain', typedData.domain)` = 0xf2cee375…, the keccak-256 of
        // 160 bytes, two blocks: the type's hash, the hashes of the name and the version,
        // the chain id and the verifying contract, 32 bytes each.
        let domain_type =
            "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&keccak256(domain_type.as_bytes()));
        encoded.extend_from_slice(&keccak256(b"Ether Mail"));
        encoded.extend_from_slice(&keccak256(b"1"));
        encoded.extend_from_slice(&bytes("0000000000000000000000000000000000000000000000000000000000000001"));
        encoded.extend_from_slice(&bytes("000000000000000000000000CcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"));
        assert_eq!(encoded.len(), 160);
        assert_eq!(
            keccak256(&encoded)[..],
            bytes("f2cee375fa42b42143804025fc449deafd50cc031ca257e0b194a650a912090f")
        );
    }
}
