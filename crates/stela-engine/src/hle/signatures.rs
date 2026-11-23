//! The signatures of the guest functions HLE replaces, generated offline
//! from a macOS build's dyld shared cache (`cargo xtask hle-signatures`),
//! one line per function naming its symbol, image and build. A line holds
//! the function's routine, length, exit offset and hashes
//! ([`Signature::generated`]), never its bytes: Stela contains no Apple
//! software. A function joins the table only once its routine has agreed
//! with it on the real bytes (`tests/hle_real.rs`).
use super::{Id, Signature};

pub static SIGNATURES: &[Signature] = &[
    // _memcpy and _memmove (one function) /usr/lib/dyld 26B123 at 0x180489280:
    // 100000 real-bytes cases agreed, 85774 run natively.
    Signature::generated(
        Id::Memmove,
        712,
        132,
        0x382aed947a65d1b0,
        [
            0xe3, 0xe1, 0x83, 0xf4, 0x6e, 0xdd, 0x81, 0xca, 0x86, 0x8b, 0xef, 0x64, 0xf1, 0x72,
            0xbe, 0xb1, 0xe2, 0x07, 0x02, 0xee, 0xd2, 0x22, 0x07, 0x60, 0x73, 0x96, 0x2b, 0xb1,
            0x48, 0xeb, 0x15, 0x10,
        ],
    ),
];
