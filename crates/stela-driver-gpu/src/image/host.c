// SPDX-License-Identifier: GPL-2.0-or-later
/* What `stela.efi` gives the image's C library (compat/libc/stela/host.h),
 * made of Linux's system calls, for a static Linux program that links the
 * image's own Mesa objects, C library and defines (`cargo xtask gpu
 * image-test`). Its render nodes are answered by Mesa's DRM shim
 * (src/drm-shim/device.c) with a kernel driver's no-op shim behind each,
 * called directly rather than through an interposed C library: the image has
 * no other. Mesa's DRM shim models one kernel driver; here each GPU's ioctls
 * run with its own driver's part of the shim's device in place, one at a
 * time, so GPUs of two kernel drivers share one process as in the image. Mappings follow host.h's contract, as Stela's GPU service does:
 * any page-aligned run of a mapping may be unmapped.
 *
 * The shims model the kernel, so what they read of the environment and the
 * clock is the real host's (image_host_option, image_host_clock_gettime),
 * never the image's C library's answers. */
#include <errno.h>
#include <stdarg.h>
#include <stdint.h>
#include <string.h>
#include <time.h>

#include <stela/host.h>

#include "drm-shim/drm_shim.h"
#include "image_host.h"

#define LINUX_WRITE 1
#define LINUX_MMAP 9
#define LINUX_MUNMAP 11
#define LINUX_CLOSE 3
#define LINUX_CLONE 56
#define LINUX_EXIT 60
#define LINUX_FTRUNCATE 77
#define LINUX_GETTID 186
#define LINUX_FUTEX 202
#define LINUX_CLOCK_GETTIME 228
#define LINUX_EXIT_GROUP 231
#define LINUX_MEMFD_CREATE 319

static long linux_call(long number, long a, long b, long c, long d, long e, long f)
{
	long result;
	register long r10 __asm__("r10") = d;
	register long r8 __asm__("r8") = e;
	register long r9 __asm__("r9") = f;
	__asm__ volatile("syscall"
			 : "=a"(result)
			 : "a"(number), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9)
			 : "rcx", "r11", "memory");
	return result;
}

static void say(const char *text)
{
	linux_call(LINUX_WRITE, 2, (long)text, (long)strlen(text), 0, 0, 0);
}

_Noreturn static void leave(int status)
{
	linux_call(LINUX_EXIT_GROUP, status, 0, 0, 0, 0, 0);
	__builtin_trap();
}

/* A spin lock for the host's own tables. */
static void lock(volatile int *word)
{
	while (__atomic_exchange_n(word, 1, __ATOMIC_ACQUIRE))
		__builtin_ia32_pause();
}

static void unlock(volatile int *word)
{
	__atomic_store_n(word, 0, __ATOMIC_RELEASE);
}

/* The heap: one reservation, handed out in order and never reused; small
 * enough for a run under a capped address space. */
#define HEAP_BYTES (4ull << 30)
static uintptr_t heap_base, heap_next;

void *stela_libc_allocate(size_t size, size_t alignment)
{
	for (;;) {
		uintptr_t next = __atomic_load_n(&heap_next, __ATOMIC_RELAXED);
		uintptr_t start = (next + alignment - 1) & ~(uintptr_t)(alignment - 1);
		if (start + size > heap_base + HEAP_BYTES)
			return NULL;
		if (__atomic_compare_exchange_n(&heap_next, &next, start + size, 0, __ATOMIC_RELAXED,
						__ATOMIC_RELAXED))
			return (void *)start;
	}
}

void stela_libc_release(void *pointer, size_t size, size_t alignment)
{
	(void)pointer;
	(void)size;
	(void)alignment;
}

void stela_libc_log(const char *text, size_t size)
{
	linux_call(LINUX_WRITE, 2, (long)text, (long)size, 0, 0, 0);
}

_Noreturn void stela_libc_fatal(const char *message)
{
	say("image host: fatal: ");
	say(message);
	say("\n");
	leave(3);
}

uint64_t stela_libc_now_ns(void)
{
	struct timespec now;
	linux_call(LINUX_CLOCK_GETTIME, CLOCK_MONOTONIC, (long)&now, 0, 0, 0, 0);
	return (uint64_t)now.tv_sec * 1000000000 + (uint64_t)now.tv_nsec;
}

uint64_t stela_libc_memory_bytes(void)
{
	return 16ull << 30;
}

uint64_t stela_libc_available_memory_bytes(void)
{
	return 12ull << 30;
}

/* Linux threads by their IDs, each with the core the C library gave it. */
#define THREADS (STELA_LIBC_THREADS + 1)
static struct {
	long tid;
	unsigned int core;
} threads[THREADS];
static volatile int threads_lock;

static void name_thread(unsigned int core)
{
	long tid = linux_call(LINUX_GETTID, 0, 0, 0, 0, 0, 0);
	lock(&threads_lock);
	for (unsigned int at = 0; at < THREADS; at++) {
		if (!threads[at].tid) {
			threads[at].tid = tid;
			threads[at].core = core;
			break;
		}
	}
	unlock(&threads_lock);
}

static void forget_thread(void)
{
	long tid = linux_call(LINUX_GETTID, 0, 0, 0, 0, 0, 0);
	lock(&threads_lock);
	for (unsigned int at = 0; at < THREADS; at++) {
		if (threads[at].tid == tid)
			threads[at].tid = 0;
	}
	unlock(&threads_lock);
}

unsigned int stela_libc_core(void)
{
	long tid = linux_call(LINUX_GETTID, 0, 0, 0, 0, 0, 0);
	unsigned int core = 0;
	lock(&threads_lock);
	for (unsigned int at = 0; at < THREADS; at++) {
		if (threads[at].tid == tid)
			core = threads[at].core;
	}
	unlock(&threads_lock);
	return core;
}

/* ANV here runs as the boot's does by default: with CCS off. */
int stela_libc_intel_ccs(void)
{
	return 0;
}

struct thread_start {
	void (*body)(void *);
	void *argument;
	unsigned int core;
	void *stack;
};

#define THREAD_STACK_BYTES (8ul << 20)

__attribute__((used, noreturn)) static void thread_main(struct thread_start *start)
{
	name_thread(start->core);
	start->body(start->argument);
	forget_thread();
	/* The stack stays: this thread still runs on it. */
	linux_call(LINUX_EXIT, 0, 0, 0, 0, 0, 0);
	__builtin_unreachable();
}

/* CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM */
#define THREAD_FLAGS 0x50f00l

int stela_libc_thread_start(void (*body)(void *), void *argument, unsigned int core)
{
	struct thread_start *start = stela_libc_allocate(sizeof(*start), 16);
	void *stack = stela_libc_allocate(THREAD_STACK_BYTES, 4096);
	if (!start || !stack)
		return -ENOMEM;
	*start = (struct thread_start){body, argument, core, stack};
	long result;
	register long r10 __asm__("r10") = 0;
	register long r8 __asm__("r8") = 0;
	register struct thread_start *r12 __asm__("r12") = start;
	__asm__ volatile("syscall\n\t"
			 "test %%rax, %%rax\n\t"
			 "jnz 1f\n\t"
			 "xor %%ebp, %%ebp\n\t"
			 "mov %%r12, %%rdi\n\t"
			 "call thread_main\n\t"
			 "1:\n\t"
			 : "=a"(result)
			 : "a"((long)LINUX_CLONE), "D"(THREAD_FLAGS),
			   "S"((char *)stack + THREAD_STACK_BYTES), "d"(0l), "r"(r10), "r"(r8), "r"(r12)
			 : "rcx", "r11", "memory");
	return result < 0 ? (int)result : 0;
}

#define FUTEX_WAIT_BITSET_PRIVATE (9 | 128)
#define FUTEX_WAKE_PRIVATE (1 | 128)

void stela_libc_wait(const volatile unsigned int *word, unsigned int value, uint64_t deadline)
{
	struct timespec until = {(time_t)(deadline / 1000000000), (long)(deadline % 1000000000)};
	linux_call(LINUX_FUTEX, (long)word, FUTEX_WAIT_BITSET_PRIVATE, value,
		   deadline == UINT64_MAX ? 0 : (long)&until, 0, -1);
}

void stela_libc_wake(const volatile unsigned int *word)
{
	linux_call(LINUX_FUTEX, (long)word, FUTEX_WAKE_PRIVATE, 0x7fffffff, 0, 0, 0);
}

/* Mappings of anonymous files, which munmap recognises by address. Every DRM
 * mapping is one too: the shim's buffers are pages of its memory file. */
#define MAPPINGS 4096
static struct {
	uintptr_t start, end;
} mappings[MAPPINGS];
static volatile int mappings_lock;

static void keep_mapping(uintptr_t start, size_t size)
{
	lock(&mappings_lock);
	for (unsigned int at = 0; at < MAPPINGS; at++) {
		if (mappings[at].start == mappings[at].end) {
			mappings[at].start = start;
			mappings[at].end = start + ((size + 4095) & ~4095ul);
			unlock(&mappings_lock);
			return;
		}
	}
	unlock(&mappings_lock);
	stela_libc_fatal("more than 4096 mappings");
}

int stela_libc_file_create(void)
{
	long fd = linux_call(LINUX_MEMFD_CREATE, (long)"image", 0, 0, 0, 0, 0);
	return fd < 0 ? (int)fd : (int)fd + STELA_LIBC_FILE_BASE;
}

int stela_libc_file_close(int fd)
{
	return (int)linux_call(LINUX_CLOSE, fd - STELA_LIBC_FILE_BASE, 0, 0, 0, 0, 0);
}

int stela_libc_file_resize(int fd, uint64_t size)
{
	return (int)linux_call(LINUX_FTRUNCATE, fd - STELA_LIBC_FILE_BASE, (long)size, 0, 0, 0, 0);
}

#define PROT_READ_WRITE 3
#define MAP_SHARED_ 0x01
#define MAP_FIXED_ 0x10
#define MAP_ANONYMOUS_ 0x20

void *stela_libc_file_mmap(int fd, uint64_t offset, size_t size, void *address)
{
	long flags = MAP_SHARED_ | (address ? MAP_FIXED_ : 0) | (fd < 0 ? MAP_ANONYMOUS_ : 0);
	long mapped = linux_call(LINUX_MMAP, (long)address, (long)size, PROT_READ_WRITE, flags,
				 fd < 0 ? -1 : fd - STELA_LIBC_FILE_BASE, (long)offset);
	if (mapped < 0 && mapped > -4096)
		return (void *)mapped;
	if (!address)
		keep_mapping((uintptr_t)mapped, size);
	return (void *)mapped;
}

/* Linux's munmap of any run: a mapping's record goes once nothing of it is
 * left; a run inside it leaves the record whole. */
int stela_libc_file_munmap(void *address, size_t size)
{
	uintptr_t start = (uintptr_t)address, end = start + ((size + 4095) & ~4095ul);
	int found = 0;
	lock(&mappings_lock);
	for (unsigned int at = 0; at < MAPPINGS; at++) {
		if (mappings[at].start == mappings[at].end || start < mappings[at].start ||
		    start >= mappings[at].end)
			continue;
		found = 1;
		if (start == mappings[at].start && end >= mappings[at].end)
			mappings[at].end = mappings[at].start;
		else if (start == mappings[at].start)
			mappings[at].start = end;
		else if (end >= mappings[at].end)
			mappings[at].end = start;
		break;
	}
	unlock(&mappings_lock);
	if (!found)
		return 0;
	long result = linux_call(LINUX_MUNMAP, (long)start, (long)(end - start), 0, 0, 0, 0);
	return result < 0 ? (int)result : 1;
}

/* The render nodes: the GPUs the program lists, each answered by the shim. */
#define DRM_FDS 256
#define FIRST_DRM_FD 1000
static struct stela_libc_drm_device listed[2];
static unsigned int listed_count;
/* The GPU of each open descriptor, or -1, and each GPU's driver ioctls. */
static int drm_gpu[DRM_FDS];
static unsigned int driver_ioctls[2];
static volatile int drm_lock;

static int drm_index(int fd)
{
	unsigned int index = (unsigned int)(fd - FIRST_DRM_FD);
	return index < DRM_FDS && drm_gpu[index] >= 0 ? (int)index : -1;
}

static int new_drm_fd(int gpu)
{
	lock(&drm_lock);
	for (unsigned int at = 0; at < DRM_FDS; at++) {
		if (drm_gpu[at] < 0) {
			drm_gpu[at] = gpu;
			unlock(&drm_lock);
			return FIRST_DRM_FD + (int)at;
		}
	}
	unlock(&drm_lock);
	return -EMFILE;
}

/* The C library names the render node's own minor, RENDER_MINOR past the device's. */
int stela_libc_drm_open(unsigned int minor)
{
	for (unsigned int gpu = 0; gpu < listed_count; gpu++) {
		if (listed[gpu].minor + 128 != minor)
			continue;
		int fd = new_drm_fd((int)gpu);
		if (fd >= 0)
			drm_shim_fd_register(fd, NULL);
		return fd;
	}
	return -ENODEV;
}

int stela_libc_drm_close(int fd)
{
	int index = drm_index(fd);
	if (index < 0)
		return -EBADF;
	drm_shim_fd_unregister(fd);
	drm_gpu[index] = -1;
	return 0;
}

int stela_libc_drm_dup(int fd)
{
	int index = drm_index(fd);
	if (index < 0)
		return -EBADF;
	int copy = new_drm_fd(drm_gpu[index]);
	if (copy >= 0)
		drm_shim_fd_register(copy, drm_shim_fd_lookup(fd));
	return copy;
}

long stela_libc_drm_rdev(int fd)
{
	int index = drm_index(fd);
	if (index < 0)
		return -EBADF;
	return (226l << 20) | (128 + listed[drm_gpu[index]].minor);
}

/* DRM's nodes seek nowhere: Linux's noop_llseek answers the position, 0. */
long long stela_libc_drm_seek(int fd, long long offset, int whence)
{
	(void)offset;
	(void)whence;
	return drm_index(fd) < 0 ? -EBADF : 0;
}

#define IOCTL_NUMBER(request) ((request) & 0xff)
#define IOCTL_TYPE(request) (((request) >> 8) & 0xff)

/* The part of the shim's device each kernel driver's no-op shim sets. */
struct personality {
	__typeof__(shim_device.iomem_region) iomem_region;
	int (**driver_ioctls)(int fd, unsigned long request, void *arg);
	int driver_ioctl_count;
	void (*driver_bo_free)(struct shim_bo *bo);
	const char *driver_name, *unique;
	int version_major, version_minor, version_patchlevel;
	int bus_type;
};

static struct personality personalities[IMAGE_KERNELS];
static enum image_kernel kernel_of[2];
static volatile int shim_lock;

static void take_personality(struct personality *personality)
{
	personality->iomem_region = shim_device.iomem_region;
	personality->driver_ioctls = shim_device.driver_ioctls;
	personality->driver_ioctl_count = shim_device.driver_ioctl_count;
	personality->driver_bo_free = shim_device.driver_bo_free;
	personality->driver_name = shim_device.driver_name;
	personality->unique = shim_device.unique;
	personality->version_major = shim_device.version_major;
	personality->version_minor = shim_device.version_minor;
	personality->version_patchlevel = shim_device.version_patchlevel;
	personality->bus_type = shim_device.bus_type;
}

static void put_personality(const struct personality *personality)
{
	shim_device.iomem_region = personality->iomem_region;
	shim_device.driver_ioctls = personality->driver_ioctls;
	shim_device.driver_ioctl_count = personality->driver_ioctl_count;
	shim_device.driver_bo_free = personality->driver_bo_free;
	shim_device.driver_name = personality->driver_name;
	shim_device.unique = personality->unique;
	shim_device.version_major = personality->version_major;
	shim_device.version_minor = personality->version_minor;
	shim_device.version_patchlevel = personality->version_patchlevel;
	shim_device.bus_type = personality->bus_type;
}

int stela_libc_drm_ioctl(int fd, unsigned long request, void *argument)
{
	int index = drm_index(fd);
	if (index < 0)
		return -EBADF;
	int gpu = drm_gpu[index];
	if (IOCTL_TYPE(request) == DRM_IOCTL_BASE && IOCTL_NUMBER(request) >= DRM_COMMAND_BASE &&
	    IOCTL_NUMBER(request) < DRM_COMMAND_END)
		__atomic_fetch_add(&driver_ioctls[gpu], 1, __ATOMIC_RELAXED);
	/* The no-op shims never wait, so one ioctl at a time costs nothing. */
	lock(&shim_lock);
	put_personality(&personalities[kernel_of[gpu]]);
	errno = 0;
	int result = drm_shim_ioctl(fd, request, argument);
	int error = errno;
	unlock(&shim_lock);
	if (result == -1 && error)
		return -error;
	return result;
}

void *stela_libc_drm_mmap(int fd, uint64_t offset, size_t size)
{
	struct shim_fd *shim = drm_index(fd) < 0 ? NULL : drm_shim_fd_lookup(fd);
	if (!shim)
		return (void *)(intptr_t)-EBADF;
	void *mapped = drm_shim_mmap(shim, size, PROT_READ_WRITE, MAP_SHARED_, fd, (off64_t)offset);
	return mapped == (void *)-1 ? (void *)(intptr_t)-EINVAL : mapped;
}

/* DRM mappings are the shim's memory file's: stela_libc_file_munmap took them. */
int stela_libc_drm_munmap(void *address, size_t size)
{
	(void)address;
	(void)size;
	return 0;
}

unsigned int stela_libc_drm_devices(void)
{
	return listed_count;
}

void stela_libc_drm_device(unsigned int index, struct stela_libc_drm_device *device)
{
	*device = listed[index];
}

/* What drm_shim.c, the shim's C library interposer, gives the device code:
 * the bus and the driver's name, and no sysfs or device files, which the
 * image never reads. */
const int render_node_minor = 128;
bool drm_shim_driver_prefers_new_render_node;

bool drm_shim_inited(void)
{
	return true;
}

void drm_shim_pci_device_setup(uint16_t vendor_id, uint16_t device_id, const char *pci_slot,
			       const char *driver_name)
{
	(void)vendor_id;
	(void)device_id;
	(void)pci_slot;
	shim_device.bus_type = DRM_BUS_PCI;
	shim_device.driver_name = driver_name;
}

void drm_shim_override_file(const char *contents, const char *path_format, ...)
{
	(void)contents;
	(void)path_format;
}

void drm_shim_override_link(const char *contents, const char *path_format, ...)
{
	(void)contents;
	(void)path_format;
}

/* The real environment, for the shims. */
static char **environment;

const char *image_host_option(const char *name)
{
	size_t length = strlen(name);
	for (char **entry = environment; entry && *entry; entry++) {
		if (!strncmp(*entry, name, length) && (*entry)[length] == '=')
			return *entry + length + 1;
	}
	return NULL;
}

int image_host_clock_gettime(clockid_t clock, struct timespec *time)
{
	long result = linux_call(LINUX_CLOCK_GETTIME, clock, (long)time, 0, 0, 0, 0);
	if (result < 0) {
		errno = (int)-result;
		return -1;
	}
	return 0;
}

/* xe's no-op shim calls Mesa's DRM shim's stat, which takes the process's;
 * here the shim's own stat is renamed aside and never called. */
int stela_drm_shim_stat(const char *path, void *buffer);
int stela_drm_shim_stat64(const char *path, void *buffer);

int stela_drm_shim_stat(const char *path, void *buffer)
{
	(void)path;
	(void)buffer;
	return -1;
}

int stela_drm_shim_stat64(const char *path, void *buffer)
{
	return stela_drm_shim_stat(path, buffer);
}

/* Each kernel driver's no-op shim, started in turn as the shim's device
 * starts, its part of the device kept for its GPUs' ioctls. */
void amdgpu_shim_driver_init(void);
void xe_shim_driver_init(void);
void i915_shim_driver_init(void);
static void (*const kernel_start[IMAGE_KERNELS])(void) = {
	[IMAGE_AMDGPU] = amdgpu_shim_driver_init,
	[IMAGE_XE] = xe_shim_driver_init,
	[IMAGE_I915] = i915_shim_driver_init,
};
static bool kernel_used[IMAGE_KERNELS];

void image_shim_driver_init(void)
{
	struct personality blank;
	take_personality(&blank);
	for (unsigned int kernel = 0; kernel < IMAGE_KERNELS; kernel++) {
		if (!kernel_used[kernel])
			continue;
		put_personality(&blank);
		kernel_start[kernel]();
		take_personality(&personalities[kernel]);
	}
}

/* i915's shim can take a device from a JSON description, which the image
 * test never gives it. */
bool image_no_device_json(const char *json, void *devinfo);

bool image_no_device_json(const char *json, void *devinfo)
{
	(void)json;
	(void)devinfo;
	return false;
}

/* The GPUs the program shows and the kernel driver that answers each. */
void image_host_list(const struct stela_libc_drm_device *gpus, const enum image_kernel *kernels,
		     unsigned int count)
{
	for (unsigned int gpu = 0; gpu < count && gpu < 2; gpu++) {
		listed[gpu] = gpus[gpu];
		kernel_of[gpu] = kernels[gpu];
		kernel_used[kernels[gpu]] = true;
	}
	listed_count = count < 2 ? count : 2;
	drm_shim_device_init();
}

/* The driver ioctls each listed GPU's descriptors were sent. */
unsigned int image_host_driver_ioctls(unsigned int gpu)
{
	return gpu < 2 ? driver_ioctls[gpu] : 0;
}

int main(int argc, char **argv);

extern void (*__init_array_start[])(void);
extern void (*__init_array_end[])(void);

__attribute__((used)) static void start(long *stack)
{
	int argc = (int)stack[0];
	char **argv = (char **)(stack + 1);
	environment = argv + argc + 1;
	heap_base = heap_next = (uintptr_t)linux_call(LINUX_MMAP, 0, (long)HEAP_BYTES, PROT_READ_WRITE,
						       0x02 | MAP_ANONYMOUS_ | 0x4000, -1, 0);
	if ((long)heap_base < 0 && (long)heap_base > -4096) {
		say("image host: no heap\n");
		leave(3);
	}
	for (unsigned int at = 0; at < DRM_FDS; at++)
		drm_gpu[at] = -1;
	name_thread(0);
	for (void (**constructor)(void) = __init_array_start; constructor < __init_array_end;
	     constructor++)
		(*constructor)();
	leave(main(argc, argv));
}

__asm__(".globl _start\n_start:\n\tmov %rsp, %rdi\n\tand $-16, %rsp\n\tcall start\n\tud2\n");
