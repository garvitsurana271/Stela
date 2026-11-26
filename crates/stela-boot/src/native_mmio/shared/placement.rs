//! Placing a memory BAR the firmware left at zero, for the driver of an
//! input controller ([`NativeServices::place_bar`]): the census's view of
//! the PCI fabric, the firmware's memory map and root bridge windows and
//! this source's reservations added to what the driver read from ACPI, and
//! [`placement::place`] choosing among them.
//!
//! [`NativeServices::place_bar`]: stela_drivers::probe::NativeServices::place_bar
use super::*;
use core::ops::Range;
use stela_drivers::bus::pci::{Address, placement};

/// Spans another reservation already holds that are stepped over before
/// a placement is given up.
const RETRIES: usize = 16;

impl Source {
    /// Reserve a span for memory BAR `index` of the function at `address`,
    /// which reads zero, inside `firmware`'s windows that overlap a memory
    /// window the firmware's root bridges report and inside every bridge
    /// window above the function, clear of `firmware`'s claims, of every
    /// entry of the memory map, of every BAR, ROM and bridge window the
    /// census read and of this source's reservations. Logs what it placed
    /// or why it placed nothing.
    ///
    /// # Safety
    /// The caller owns the function and programs the returned span into
    /// its BAR, with no other owner decoding there.
    pub(crate) unsafe fn place_bar(
        &self,
        address: Address,
        index: u8,
        request: placement::Request,
        firmware: &placement::Firmware,
    ) -> Result<Resource, &'static str> {
        let width = if request.wide { "64-bit" } else { "32-bit" };
        let refuse = |why: &str, short: &'static str| {
            crate::output::arguments(format_args!(
                "HOST PCI {address} BAR{index} ({:#x} bytes, {width}) left unassigned by the \
                 firmware and not placed: {why}\n",
                request.bytes
            ));
            short
        };
        let fabric = self
            .fabric_memory(address)
            .map_err(|why| refuse(&why, "the PCI census cannot vouch for the free space"))?;
        // The firmware's own BAR is never moved (`placement::unassigned`).
        let at = usize::from(index);
        let low = fabric.own.get(at).copied().unwrap_or(u32::MAX);
        let high = if request.wide {
            fabric.own.get(at + 1).copied().unwrap_or(u32::MAX)
        } else {
            0
        };
        if index >= 6 || low & !0xf != 0 || high != 0 {
            return Err(refuse(
                &alloc::format!(
                    "the census read it at {:#x}: the firmware's",
                    u64::from(high) << 32 | u64::from(low & !0xf)
                ),
                "the BAR is not unassigned",
            ));
        }
        let wire = self.platform.wire();
        let roots: Vec<Range<u64>> = wire
            .roots()
            .iter()
            .filter(|root| {
                root.space == 0
                    && root.translation == 0
                    && root.segment == u32::from(address.segment)
            })
            .filter_map(|root| placement::inclusive(root.minimum, root.maximum))
            .collect();
        // A `_CRS` window counts only where the firmware's own root bridge
        // setup says the same root bridge forwards.
        let mut windows: Vec<Range<u64>> = firmware
            .windows
            .iter()
            .filter(|window| {
                roots
                    .iter()
                    .any(|root| root.start < window.end && window.start < root.end)
            })
            .cloned()
            .collect();
        if windows.is_empty() {
            return Err(refuse(
                "no memory window of its root bridge's _CRS holds a window the firmware's root \
                 bridges report",
                "no root bridge window",
            ));
        }
        for bridge in &fabric.upstream {
            windows = placement::intersect(&windows, &bridge.admitting(request));
        }
        let mut claimed = firmware.claimed.clone();
        claimed.extend(fabric.claimed);
        claimed.extend(wire.memory().iter().map(|entry| entry.start..entry.end));
        let kind = if request.prefetchable {
            ResourceKind::PrefetchableMemory
        } else {
            ResourceKind::Registers
        };
        for _ in 0..RETRIES {
            let span = match placement::place(&windows, &claimed, request) {
                Ok(span) => span,
                Err(refusal) => {
                    let mut listed = String::new();
                    for window in windows.iter().take(8) {
                        listed.push_str(&alloc::format!(
                            " {:#x}-{:#x}",
                            window.start,
                            window.end - 1
                        ));
                    }
                    return Err(refuse(
                        &alloc::format!(
                            "{refusal} (windows:{listed}; {} claimed ranges)",
                            claimed.len()
                        ),
                        "no free span",
                    ));
                }
            };
            let physical = PhysicalSpan::new(span.start, span.end - span.start)
                .map_err(|_| refuse("its span is no physical range", "invalid span"))?;
            match self.reserve(physical, kind) {
                Ok(resource) => {
                    let window = windows
                        .iter()
                        .find(|window| window.start <= span.start && span.end <= window.end)
                        .cloned()
                        .unwrap_or(span.clone());
                    crate::output::arguments(format_args!(
                        "HOST PCI {address} BAR{index} ({:#x} bytes, {width}) left unassigned by \
                         the firmware: placed at {:#x}-{:#x}, in window {:#x}-{:#x}, clear of {} \
                         claimed ranges\n",
                        request.bytes,
                        span.start,
                        span.end - 1,
                        window.start,
                        window.end - 1,
                        claimed.len()
                    ));
                    return Ok(resource);
                }
                // Another owner holds it: step over it.
                Err(Error::ResourceConflict) => claimed.push(span),
                Err(error) => {
                    return Err(refuse(
                        &alloc::format!("reserving {:#x}: {error:?}", span.start),
                        "the reservation was refused",
                    ));
                }
            }
        }
        Err(refuse(
            "every span it found was another owner's",
            "no free span",
        ))
    }
}
