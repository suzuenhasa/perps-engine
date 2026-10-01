//! A MessagePack writer for the few forms EIP-712 signing needs (`docs/PIPELINE.md` 5.8;
//! `docs/DECISIONS.md` D-033). Our own, in safe Rust: no new crate, and no allocation.
//!
//! **Why.** Polymarket signs the keccak-256 of an operation's MessagePack encoding
//! (`eip712.rs`), so our bytes must be exactly the bytes their SDKs' encoders write
//! (`@msgpack/msgpack` in TypeScript, `msgpack.packb` in Python), and those always pick the
//! smallest form of each value. A MessagePack value is a type byte, sometimes followed by a
//! length or by the value itself, big-endian. The forms written here, from the MessagePack
//! specification (github.com/msgpack/msgpack, `spec.md`):
//!
//! | Value | Form | Bytes |
//! |---|---|---|
//! | array of `n` < 16 values | fixarray | `0x90 + n` |
//! | array of `n` < 65,536 values | array 16 | `0xdc`, `n` as a `u16` |
//! | string of `n` < 32 bytes | fixstr | `0xa0 + n`, then the bytes |
//! | string of `n` < 256 bytes | str 8 | `0xd9`, `n` as a `u8`, then the bytes |
//! | string of `n` < 65,536 bytes | str 16 | `0xda`, `n` as a `u16`, then the bytes |
//! | integer `v` < 128 | positive fixint | `v` |
//! | integer `v` < 256 | uint 8 | `0xcc`, `v` as a `u8` |
//! | integer `v` < 65,536 | uint 16 | `0xcd`, `v` as a `u16` |
//! | integer `v` < 2^32 | uint 32 | `0xce`, `v` as a `u32` |
//! | any other `u64` | uint 64 | `0xcf`, `v` as a `u64` |
//! | `false`, `true` | | `0xc2`, `0xc3` |
//!
//! An array's header gives only its count; its values follow, each written by its own call.
//! Nothing else is ever signed: no maps, negative integers, floats, nil or binary.
//!
//! **Contract.** A [`Writer`] appends to a byte buffer its caller owns (a stack array), and
//! [`Writer::finish`] returns the bytes written. It panics if the buffer is too small (the
//! caller sizes it for its longest form, `eip712::MAX_OP_BYTES`) or if an array or a string
//! is longer than 65,535; both would be bugs, never something a message can cause.
//!
//! **Complexity.** O(1) per value, plus the copy of a string's bytes.

/// Writes MessagePack values into a caller's buffer (module docs).
#[derive(Debug)]
pub struct Writer<'a> {
    buffer: &'a mut [u8],
    /// Bytes written so far, from the start of `buffer`.
    len: usize,
}

impl<'a> Writer<'a> {
    /// A writer that starts at the beginning of `buffer`.
    pub fn new(buffer: &'a mut [u8]) -> Writer<'a> {
        Writer { buffer, len: 0 }
    }

    /// The header of an array of `count` values; the values follow, one call each.
    pub fn array(&mut self, count: usize) {
        match count {
            0..16 => self.put(&[0x90 + count as u8]),
            16..0x1_0000 => {
                self.put(&[0xDC]);
                self.put(&(count as u16).to_be_bytes());
            }
            _ => panic!("an array of {count} values: this writer stops at 65,535"),
        }
    }

    /// A string: `text` is its UTF-8 bytes (ours are all ASCII).
    pub fn str(&mut self, text: &[u8]) {
        let len = text.len();
        match len {
            0..32 => self.put(&[0xA0 + len as u8]),
            32..0x100 => self.put(&[0xD9, len as u8]),
            0x100..0x1_0000 => {
                self.put(&[0xDA]);
                self.put(&(len as u16).to_be_bytes());
            }
            _ => panic!("a string of {len} bytes: this writer stops at 65,535"),
        }
        self.put(text);
    }

    /// A non-negative integer, in the smallest form that holds it.
    pub fn uint(&mut self, value: u64) {
        match value {
            0..0x80 => self.put(&[value as u8]),
            0x80..0x100 => self.put(&[0xCC, value as u8]),
            0x100..0x1_0000 => {
                self.put(&[0xCD]);
                self.put(&(value as u16).to_be_bytes());
            }
            0x1_0000..0x1_0000_0000 => {
                self.put(&[0xCE]);
                self.put(&(value as u32).to_be_bytes());
            }
            _ => {
                self.put(&[0xCF]);
                self.put(&value.to_be_bytes());
            }
        }
    }

    /// `false` or `true`.
    pub fn bool(&mut self, value: bool) {
        self.put(&[if value { 0xC3 } else { 0xC2 }]);
    }

    /// The bytes written, from the start of the buffer.
    pub fn finish(self) -> &'a [u8] {
        &self.buffer[..self.len]
    }

    /// Appends `bytes`; panics past the end of the buffer (module docs, "Contract").
    fn put(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        self.buffer[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keccak::keccak256;
    use crate::test_support::bytes;

    /// What `write` writes, into a buffer big enough for every test here.
    fn written(write: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut buffer = [0u8; 70_000];
        let mut writer = Writer::new(&mut buffer);
        write(&mut writer);
        writer.finish().to_vec()
    }

    #[test]
    fn integers_take_the_smallest_form_at_every_boundary() {
        for (value, expected) in [
            (0, "00"),
            (127, "7f"),
            (128, "cc 80"),
            (255, "cc ff"),
            (256, "cd 01 00"),
            (65_535, "cd ff ff"),
            (65_536, "ce 00 01 00 00"),
            (u64::from(u32::MAX), "ce ff ff ff ff"),
            (1 << 32, "cf 00 00 00 01 00 00 00 00"),
            (u64::MAX, "cf ff ff ff ff ff ff ff ff"),
        ] {
            assert_eq!(written(|w| w.uint(value)), bytes(expected), "{value}");
        }
    }

    #[test]
    fn strings_and_arrays_take_the_smallest_form_at_every_boundary() {
        for (len, header) in [(0, "a0"), (31, "bf"), (32, "d9 20"), (255, "d9 ff"), (256, "da 01 00")] {
            let text = vec![b'x'; len];
            let mut expected = bytes(header);
            expected.extend_from_slice(&text);
            assert_eq!(written(|w| w.str(&text)), expected, "a string of {len} bytes");
        }
        assert_eq!(written(|w| w.str(&[b'x'; 65_535]))[..3], bytes("da ff ff"));
        for (count, header) in [(0, "90"), (15, "9f"), (16, "dc 00 10"), (65_535, "dc ff ff")] {
            assert_eq!(written(|w| w.array(count)), bytes(header), "an array of {count}");
        }
        assert_eq!(written(|w| w.bool(false)), [0xC2]);
        assert_eq!(written(|w| w.bool(true)), [0xC3]);
    }

    #[test]
    #[should_panic(expected = "this writer stops at 65,535")]
    fn a_string_of_65536_bytes_is_refused() {
        written(|w| w.str(&[b'x'; 65_536]));
    }

    #[test]
    #[should_panic]
    fn writing_past_the_end_of_the_buffer_panics() {
        let mut buffer = [0u8; 3];
        let mut writer = Writer::new(&mut buffer);
        writer.str(b"abc");
    }

    /// One golden vector: its bytes in hex, how the writer writes them, its `data` (with
    /// `0x`) and its name in the file.
    type GoldenBytes = (&'static str, fn(&mut Writer), &'static str, &'static str);

    /// Polymarket's golden vectors 1, 3, 4, 6 and 7 (py-sdk
    /// `tests/unit/test_perps_signing_golden.py`), as bytes. The file gives each vector's
    /// operation and the keccak-256 of its bytes (`data`), not the bytes themselves, so each
    /// check here is twice over: the writer writes these bytes, and they hash to the
    /// vector's `data`.
    #[test]
    fn the_writer_writes_the_bytes_of_polymarkets_golden_vectors() {
        let cases: [GoldenBytes; 5] = [
            (
                // ["createOrders", [[1, True, "0.5", "10", "gtc", False, None, None, None]]]
                "92 ac 6372656174654f7264657273 91 96 01 c3 a3 302e35 a2 3130 a3 677463 c2",
                |w| {
                    w.array(2);
                    w.str(b"createOrders");
                    w.array(1);
                    w.array(6); // the three Nones are dropped, not written as nil
                    w.uint(1);
                    w.bool(true);
                    w.str(b"0.5");
                    w.str(b"10");
                    w.str(b"gtc");
                    w.bool(false);
                },
                "0x8004f264b573f0d5edd3377ef127f251a2b11e0b9463c5fb5f1be3b42c94336a",
                "createOrders single GTC",
            ),
            (
                // ["createOrders", [[7, True, "100", "3", "gtc", False, None, None, None],
                //   [7, False, None, "3", None, False, True, None, [True, "200", "tp"]],
                //   [7, False, "50", "3", None, False, True, None, [None, "49", "sl"]]], "order"]
                "93 ac 6372656174654f7264657273 93
                 96 07 c3 a3 313030 a1 33 a3 677463 c2
                 96 07 c2 a1 33 c2 c3 93 c3 a3 323030 a2 7470
                 97 07 c2 a2 3530 a1 33 c2 c3 92 a2 3439 a2 736c
                 a5 6f72646572",
                |w| {
                    w.array(3);
                    w.str(b"createOrders");
                    w.array(3);
                    w.array(6);
                    w.uint(7);
                    w.bool(true);
                    w.str(b"100");
                    w.str(b"3");
                    w.str(b"gtc");
                    w.bool(false);
                    w.array(6); // Nones dropped wherever they are, not just at the end
                    w.uint(7);
                    w.bool(false);
                    w.str(b"3");
                    w.bool(false);
                    w.bool(true);
                    w.array(3);
                    w.bool(true);
                    w.str(b"200");
                    w.str(b"tp");
                    w.array(7);
                    w.uint(7);
                    w.bool(false);
                    w.str(b"50");
                    w.str(b"3");
                    w.bool(false);
                    w.bool(true);
                    w.array(2); // inside nested arrays too
                    w.str(b"49");
                    w.str(b"sl");
                    w.str(b"order");
                },
                "0x34b31ff82aa6d39bd6ee54a7781287d6e6dffcc0cc7dcc981f70e381c7d8a641",
                "createOrders grouped with tpsl triggers",
            ),
            (
                // ["cancelOrders", [11, 22, 33]]
                "92 ac 63616e63656c4f7264657273 93 0b 16 21",
                |w| {
                    w.array(2);
                    w.str(b"cancelOrders");
                    w.array(3);
                    w.uint(11);
                    w.uint(22);
                    w.uint(33);
                },
                "0x13f4659952efbcd144324c0a1c50cb75069635b2bbf28d3cc1e51b6e176a7617",
                "cancelOrders",
            ),
            (
                // ["updateLeverage", [3, 20, True]]
                "92 ae 7570646174654c65766572616765 93 03 14 c3",
                |w| {
                    w.array(2);
                    w.str(b"updateLeverage");
                    w.array(3);
                    w.uint(3);
                    w.uint(20);
                    w.bool(true);
                },
                "0x1daa002ce2f4e42ab0ef2d3c489b2386cccac2cbb861bdd7695761d4d93dd410",
                "updateLeverage",
            ),
            (
                // ["autoCancel", [1767000045000]]: at least 2^32, so a uint 64
                "92 aa 6175746f43616e63656c 91 cf 0000019b6968f5c8",
                |w| {
                    w.array(2);
                    w.str(b"autoCancel");
                    w.array(1);
                    w.uint(1_767_000_045_000);
                },
                "0x8d16f1dbf6be71cea6ad70c5b09f028dd1b451cebb2c7dd20c82dbf1022440ba",
                "autoCancel arm",
            ),
        ];
        for (hex, write, data, name) in cases {
            let expected = bytes(hex);
            assert_eq!(written(write), expected, "{name}: bytes");
            assert_eq!(keccak256(&expected)[..], bytes(&data[2..]), "{name}: data");
        }
    }
}
