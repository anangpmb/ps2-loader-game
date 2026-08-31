//! OPL's non-standard CRC32, used to name USBExtreme chunk files.
//!
//! This is NOT a standard CRC32 — it is a direct port of `USBA_crc32` from
//! Open PS2 Loader's `system.c`. The chunk filename for a split game is
//! `ul.<CRC32 of the display name>.<gameId>.<part>`, where this exact algorithm
//! computes the CRC. Getting this byte-identical matters: OPL recomputes the CRC
//! from the `ul.cfg` name field to locate the chunk files, so a mismatch means
//! the game is invisible on the console.
//!
//! Ported from the C# reference `OplCrc32.cs` (PS2IsoManager), which is verified
//! against the OPL source. Signed `i32` arithmetic is used deliberately so the
//! left-shift / MSB behaviour matches the reference bit-for-bit. Rust's `<<`
//! does not overflow-panic (only shift-count >= bit-width panics), so the wrap
//! is well defined.

use std::sync::OnceLock;

/// Note: `crctab[i]` is stored at reversed index `255 - table` on purpose —
/// that is how OPL builds its table.
fn table() -> &'static [u32; 256] {
    static TAB: OnceLock<[u32; 256]> = OnceLock::new();
    TAB.get_or_init(|| {
        let mut crctab = [0u32; 256];
        for table in 0..256i32 {
            let mut crc: i32 = table << 24;
            for _ in 0..8 {
                if crc < 0 {
                    crc = (crc << 1) ^ 0x04C1_1DB7;
                } else {
                    crc <<= 1;
                }
            }
            crctab[(255 - table) as usize] = crc as u32;
        }
        crctab
    })
}

/// Compute OPL's CRC32 of a game's display name.
pub fn crc32(name: &str) -> u32 {
    let tab = table();

    // Name is copied into a 33-byte, null-padded ASCII buffer (max 32 chars).
    let mut buffer = [0u8; 33];
    let name_bytes = name.as_bytes();
    let n = name_bytes.len().min(32);
    buffer[..n].copy_from_slice(&name_bytes[..n]);

    let mut crc: i32 = 0;
    let mut count: usize = 0;
    loop {
        let b = buffer[count] as i32;
        count += 1;
        crc = (tab[(b ^ ((crc >> 24) & 0xFF)) as usize] ^ (((crc as u32) << 8) & 0xFFFF_FF00)) as i32;
        // Stop after processing the null terminator, or after 32 bytes.
        if buffer[count - 1] == 0 || count > 32 {
            break;
        }
    }

    crc as u32
}

/// Uppercase 8-digit hex form used in chunk filenames (`ul.<HEX>.<id>.<part>`).
pub fn crc32_hex(name: &str) -> String {
    format!("{:08X}", crc32(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        assert_eq!(crc32("God of War"), crc32("God of War"));
        assert_ne!(crc32("God of War"), crc32("Shadow of the Colossus"));
    }

    #[test]
    fn hex_is_8_uppercase_digits() {
        let h = crc32_hex("Some Game");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }

    #[test]
    fn empty_name_matches_reference() {
        // For an empty name the loop processes a single null byte (b=0, crc=0):
        // result = tab[(0 ^ 0) as usize] ^ 0 = tab[0].
        // tab[0] = crctab[255], built from table=0: crc starts at 0, all 8 left-shifts
        // stay 0 (MSB never set), so crctab[255] = 0. Therefore crc32("") = 0.
        assert_eq!(crc32(""), 0);
    }
}
