//! Linux's command line for the GPU drivers: the words that set built-in
//! modules' parameters before any of them registers ([`crate::stack::start_kernel`]).
//! Linux refuses a word no built-in module declares, and that refusal fails
//! the whole start, so each word is checked against the parameters the
//! image's objects register, and one they do not is left out by name.
use alloc::{format, string::String, vec::Vec};

/// Every `module.parameter` the image registers: the name of each module
/// parameter its captured units declare, read from those objects (the ones
/// the image links) by the build (`build/image.rs`).
pub const REGISTERED: &[&str] = include!(concat!(env!("OUT_DIR"), "/parameters.rs"));

/// TTM's limit for an integrated GPU whose share of RAM is `share` pages,
/// which Linux also sees as all its RAM (the page catalog's limit).
///
/// The limit is twice the share. amdgpu sizes its GTT by TTM's limit and
/// caps an APU's at the RAM Linux sees, so its GTT is the share; with the
/// default, half the RAM, it was half the share. And TTM swaps buffers out
/// to shmem whenever the pages it counts, with those a buffer is about to
/// take, pass its limit (`ttm_tt_populate`). Here there is no swap, and
/// shmem would be RAM of the same share, so that path must never run. The
/// pages TTM holds are at most the share, as the page catalog refuses more,
/// and a buffer's pages at most its GTT's size, the share again, so no
/// count passes twice the share: a buffer the share cannot hold fails its
/// allocation from the catalog instead.
pub fn page_limit(share: usize) -> String {
    format!("ttm.pages_limit={}", share.saturating_mul(2))
}

/// TTM's limit where no share is planned: the bound on Linux's pages,
/// `pages` (the page catalog's limit, what the heap holds past its own
/// needs). A discrete GPU's GTT is TTM's limit (amdgpu caps only an APU's),
/// so it holds the pages the heap can give. TTM's count passes it only
/// where Linux's pages would pass the catalog's limit too; its swap to
/// shmem is then refused, and the allocation fails as it would at the
/// catalog.
pub fn bound(pages: usize) -> String {
    format!("ttm.pages_limit={pages}")
}

/// The firmware file the Intel display code of `driver` (`xe` or `i915`,
/// each of which builds it as its own module's) asks for the video BIOS
/// table before it looks anywhere else (`intel_bios.c`).
pub fn video_bios_table(driver: &str, file: &str) -> String {
    format!("{driver}.vbt_firmware={file}")
}

/// An empty path for the HuC image of Intel `driver` (`xe` or `i915`),
/// which turns its media controller off (`xe_uc_fw.c`, `intel_uc_fw.c`).
pub fn without_huc(driver: &str) -> String {
    format!("{driver}.huc_firmware_path=")
}

/// i915 without its GuC (`enable_guc=0`): execlists submission and its own
/// frequency control, no HuC (`intel_uc.c`).
pub fn without_guc(driver: &str) -> String {
    format!("{driver}.enable_guc=0")
}

/// The Intel display features off that keep a CPU write to the scanned-out
/// buffer from reaching the panel without a flush i915's front-buffer
/// tracking sees (DIRTYFB, a page flip): panel self refresh and panel replay,
/// where the panel shows its own copy of the last frame, and framebuffer
/// compression, where the plane scans out a compressed copy. Stela's console
/// draws the display's front buffer with the CPU and flushes nothing, so on
/// dev64's eDP panel every line after the display's take, the stop's reason
/// among them, never showed.
pub fn front_buffer_shown(driver: &str) -> [String; 3] {
    [
        format!("{driver}.enable_psr=0"),
        format!("{driver}.enable_panel_replay=0"),
        format!("{driver}.enable_fbc=0"),
    ]
}

/// An empty path for the GSC image of Intel `driver`, which turns its
/// security controller off.
pub fn without_gsc(driver: &str) -> String {
    format!("{driver}.gsc_firmware_path=")
}

/// The masks amdgpu reads once for every GPU it drives, whose bits are the
/// link speeds and widths it may use ([`crate::amdgpu::link`]).
const LINK_MASKS: [&str; 2] = ["amdgpu.pcie_gen_cap", "amdgpu.pcie_lane_cap"];

/// Adds `word` to `words` unless it is there already. One of amdgpu's link
/// masks that another GPU set differently keeps the bits both set, the
/// narrower link of the two: Linux's last word would give one GPU a link
/// faster than its slot's.
pub fn add(words: &mut Vec<String>, word: &str) {
    if words.iter().any(|seen| seen == word) {
        return;
    }
    let mask = |word: &str| {
        let (name, value) = word.split_once('=')?;
        let value = u32::from_str_radix(value.strip_prefix("0x")?, 16).ok()?;
        LINK_MASKS
            .contains(&name)
            .then_some((String::from(name), value))
    };
    if let Some((name, value)) = mask(word)
        && let Some((seen, held)) = words.iter_mut().find_map(|seen| {
            mask(seen)
                .filter(|(other, _)| *other == name)
                .map(|(_, held)| (seen, held))
        })
    {
        *seen = format!("{name}={:#x}", held & value);
        return;
    }
    words.push(String::from(word));
}

/// Whether a parameter the image registers takes `word`. Linux matches
/// names with `-` and `_` alike (`parameq`).
pub fn registered(word: &str) -> bool {
    let name = word.split_once('=').map_or(word, |(name, _)| name);
    let same = |a: u8, b: u8| a == b || matches!((a, b), (b'-', b'_') | (b'_', b'-'));
    REGISTERED.iter().any(|known| {
        known.len() == name.len() && known.bytes().zip(name.bytes()).all(|(a, b)| same(a, b))
    })
}

/// The command line of `words`, in order, and the words left out of it
/// because no parameter the image registers takes them.
pub fn compose(words: impl IntoIterator<Item = String>) -> (String, Vec<String>) {
    let (kept, left_out): (Vec<String>, Vec<String>) =
        words.into_iter().partition(|word| registered(word));
    (kept.join(" "), left_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Every word the boot can put on the command line names a parameter
    /// the built objects register, so none is refused or left out.
    #[test]
    fn every_word_the_boot_builds_is_registered() {
        let mut words = vec![page_limit(655_360), bound(224_256)];
        // Only an image with an Intel driver holds its parameters: its own
        // (i915's in `i915_params.c`, xe's in `xe_module.c`) and those of the
        // display code it builds.
        for (driver, built) in [
            ("xe", cfg!(feature = "xe-port")),
            ("i915", cfg!(feature = "i915-port")),
        ] {
            if built {
                words.extend([
                    video_bios_table(driver, "intel/vbt.bin"),
                    without_huc(driver),
                    without_gsc(driver),
                ]);
                words.extend(front_buffer_shown(driver));
                if driver == "i915" {
                    words.push(without_guc(driver));
                }
            }
        }
        // amdgpu's link masks for a discrete GPU (`amdgpu_drv.c`).
        #[cfg(feature = "amdgpu-port")]
        words.extend(
            crate::amdgpu::link::Link {
                generation: 4,
                width: 16,
            }
            .parameters()
            .split_whitespace()
            .map(String::from),
        );
        let (line, left_out) = compose(words.clone());
        assert!(
            left_out.is_empty(),
            "not registered: {left_out:?} of {REGISTERED:?}"
        );
        assert_eq!(line, words.join(" "));
    }

    #[test]
    fn an_unknown_word_is_left_out_by_name() {
        let (line, left_out) = compose([
            String::from("ttm.no_such_parameter=1"),
            page_limit(8),
            String::from("bare"),
        ]);
        assert_eq!(line, "ttm.pages_limit=16");
        assert_eq!(left_out, ["ttm.no_such_parameter=1", "bare"]);
        assert!(registered("ttm.pages-limit=1"));
    }

    /// A word is added once; two GPUs' link masks keep the narrower link,
    /// whichever comes first.
    #[test]
    fn two_gpus_link_masks_keep_the_narrower_link() {
        let mut words = Vec::new();
        for word in [
            "ttm.pages_limit=8",
            "amdgpu.pcie_gen_cap=0xf000f",
            "amdgpu.pcie_lane_cap=0x3f003f",
            "ttm.pages_limit=8",
            "amdgpu.pcie_gen_cap=0x70007",
            "amdgpu.pcie_lane_cap=0x3f003f",
            "amdgpu.pcie_lane_cap=0xf000f",
            "amdgpu.pcie_gen_cap=0xf000f",
        ] {
            add(&mut words, word);
        }
        assert_eq!(
            words,
            [
                "ttm.pages_limit=8",
                "amdgpu.pcie_gen_cap=0x70007",
                "amdgpu.pcie_lane_cap=0xf000f"
            ]
        );
        // Another word keeps Linux's own rule: added as it is.
        add(&mut words, "ttm.pages_limit=16");
        assert_eq!(words.last().map(String::as_str), Some("ttm.pages_limit=16"));
    }

    /// A share's limit is twice it; a bound is itself.
    #[test]
    fn a_bound_is_ttms_limit_as_it_is() {
        assert_eq!(page_limit(655_360), "ttm.pages_limit=1310720");
        assert_eq!(bound(224_256), "ttm.pages_limit=224256");
    }
}
