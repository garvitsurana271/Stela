/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef STELA_PCI_H
#define STELA_PCI_H
#include <linux/pci.h>

/* A function its owner built, on a bus with the owner's pci_ops. Publish an
 * already initialized/enrolled device; removal drops its registration ref. */
int stela_linux_pci_publish_device(struct pci_dev *dev);
/* Publish an initialized device for other drivers' searches only: no driver
 * is offered it, now or when one registers later. */
int stela_linux_pci_publish_for_searches(struct pci_dev *dev);
bool stela_linux_pci_devices_present(void);
/* Checked original standard capability walker. PCIBIOS failure is distinct
 * from a successful absent capability (*found == 0). No context ownership moves. */
int stela_linux_pci_find_capability(struct pci_dev *dev, int cap, u8 *found);
bool stela_linux_pci_bound(const struct pci_dev *device);
/* Whether the owner of an attached function routes its MSI-X table, and
 * how many vectors it has lines for (0 for none or no table). */
bool stela_linux_pci_routes_msix(const struct pci_dev *device);
int stela_linux_pci_msix_lines(struct pci_dev *device);
void stela_linux_pci_shutdown(struct pci_dev *dev);
void stela_linux_pci_remove_device(struct pci_dev *dev);

/* A placed memory region of a function. */
struct stela_linux_pci_region {
	u64 start, size;
	unsigned long flags;
};

/* A physical function its owner enumerated. Configuration accesses return a
 * PCIBIOS result; dma_map returns DMA_MAPPING_ERROR when it cannot map, and
 * dma_limit is the highest address it hands out. Rust retains exact typed
 * access causes in the actual State; a possibly applied failed write traps
 * before returning to an original caller that could ignore a PCIBIOS failure.
 * Real hardware all-ones with a successful access remains success/data. */
struct stela_linux_pci_function {
	void *context;
	int (*read_config)(void *context, int where, int size, u32 *value);
	int (*write_config)(void *context, int where, int size, u32 value);
	dma_addr_t (*dma_map)(void *context, phys_addr_t physical, size_t size,
			      enum dma_data_direction direction, bool pageless);
	void (*dma_unmap)(void *context, dma_addr_t address, size_t size,
			  enum dma_data_direction direction);
	/* Move the function's interrupt from its line to a message and back. The
	 * irq stays the name Linux knows it by; enable returns a negative errno. */
	int (*enable_msi)(void *context);
	void (*disable_msi)(void *context);
	u64 dma_limit;
	unsigned int bus, devfn, irq;
	struct stela_linux_pci_region regions[PCI_STD_NUM_BARS];
	/* Optional ownership transfer: successful attach keeps context through
	 * the final device reference, including references beyond removal.
	 * Called exactly once at quiescent process release in the attach owner.
	 * Nonquiescent or other-owner final release quarantines host/context;
	 * no destructor is invoked from atomic or private RCU callback context. */
	void (*release_context)(void *context);
	/* Original initialized device enrollment; called before any driver probe. */
	int (*enroll)(void *context, struct device *device);
	/* The function's platform ROM, a video BIOS image firmware publishes
	 * apart from the function (as x86 Linux takes the one EFI hands over),
	 * which the attach copies into pci_dev::rom: rom_length 0 for none. */
	const void *rom;
	size_t rom_length;
	/* The expansion ROM BAR where the owner placed it, decoding off, which
	 * the attach keeps as PCI_ROM_RESOURCE: size 0 for none. */
	struct stela_linux_pci_region rom_bar;
	/* Optional MSI-X, NULL where the owner routes none: set up to `vectors`
	 * entries of the function's MSI-X table, each masked until its line is
	 * enabled, on consecutive lines from *first_irq. Returns how many it set
	 * up (fewer where its lines run out) or a negative errno. Called again
	 * while enabled, it writes the same entries back, as after a reset.
	 * `vectors` 0 sets up nothing and returns how many it could. */
	int (*enable_msix)(void *context, unsigned int vectors, unsigned int *first_irq);
	void (*disable_msix)(void *context);
};

_Static_assert(sizeof(struct stela_linux_pci_function) == 296, "Linux PCI function ABI");
_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, enable_msix) == 280,
	       "Linux PCI MSI-X ABI");
_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, rom_bar) == 256,
	       "Linux PCI ROM BAR ABI");
_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, release_context) == 224,
	       "Linux PCI context release ABI");

_Static_assert(__builtin_offsetof(struct stela_linux_pci_function, enroll) == 232,
	       "Linux PCI enrollment ABI");

/* Present function to Linux, which binds any driver matching it. NULL reports
 * a negative errno (if error is non-NULL), without taking context ownership. Removal unregisters the device
 * and drops the registration reference; other references keep callbacks live.
 * The attach owner/platform and callback code must outlive every escaped
 * device reference or quarantined final release. */
struct pci_dev *stela_linux_pci_attach(const struct stela_linux_pci_function *function, int *error);
/* What identifies a function its owner keeps to itself: enough for another
 * function's driver to find it by address, class or ID and read who it is. */
struct stela_linux_pci_identity {
	unsigned int bus, devfn;
	u16 vendor, device, subsystem_vendor, subsystem_device;
	/* Configuration register 8: the class in the upper 24 bits. */
	u32 class_revision;
};

_Static_assert(sizeof(struct stela_linux_pci_identity) == 20, "Linux PCI identity ABI");

/* Present a function to searches only (pci_get_class, pci_dev_present,
 * pci_get_domain_bus_and_slot). No driver binds it, it has no resources,
 * interrupt or DMA, its configuration reads fail as for an absent function and
 * a write is refused and reported once. NULL reports a negative errno.
 * stela_linux_pci_remove_device drops the registration reference. */
struct pci_dev *stela_linux_pci_publish_identity(const struct stela_linux_pci_identity *identity,
						 int *error);
/* As stela_linux_pci_publish_identity, with a copy of the first length bytes
 * (at most 256) of the function's configuration as its owner read them:
 * reads inside the copy answer its bytes, others fail as for an absent
 * function, and a write is refused and reported once. */
struct pci_dev *stela_linux_pci_publish_snapshot(const struct stela_linux_pci_identity *identity,
						 const u8 *config, size_t length, int *error);
/* Cumulative final releases quarantined outside the original process owner.
 * Atomic observation only; no reset or deferred reclamation is implied. */
u64 stela_linux_pci_quarantined_functions(void);
/* An attached function's platform ROM copy, which ioremap answers with its
 * kernel address when [start, start + size) lies in it, or NULL; and whether
 * an address iounmap is given is one of those answers. */
void __iomem *stela_linux_pci_rom_map(phys_addr_t start, size_t size);
bool stela_linux_pci_rom_mapped(const volatile void __iomem *address);
/* Cache what Linux's probe reads from a PCI Express function's capability:
 * its flags, device capabilities and supported link speeds. A PCIBIOS error
 * when the function does not answer. */
int stela_linux_pci_express_probe(struct pci_dev *dev);
#endif
