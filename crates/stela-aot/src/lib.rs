//! Whole-image analysis for Stambha's ahead-of-time translator.
//!
//! About 95% of the guest time a macOS boot spends runs in images that are
//! byte-identical for one macOS build: the dyld shared cache, the kernel
//! collection, SPTM and TXM. Unlike the JIT, which sees one block at a time
//! as the guest reaches it, an ahead-of-time translator can read each image
//! whole: its function starts, every function's control-flow graph, which
//! registers each function uses and where they are live, and who calls
//! whom. This crate reads the images ([`image`]), classifies every word
//! ([`a64`]), builds each function's graph and liveness ([`cfg`]) and
//! counts what a translation could take ([`census`]). The translation
//! itself is `stela_compiler`'s (`block::aot`), which compiles one function
//! unit at a time with the JIT's own emitters.
pub mod a64;
pub mod census;
pub mod cfg;
pub mod image;
