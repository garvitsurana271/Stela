// SPDX-License-Identifier: GPL-2.0-only
/* The host a test of the C library's users runs on: Linux, by its system calls,
 * with one processor. */
#include "host.h"
#include <ctype.h>
#include <errno.h>
#include <stela/host.h>
#include <string.h>
#include <wchar.h>

static long linux_call(long number, long a, long b, long c)
{
	long result;
	__asm__ volatile("syscall" : "=a"(result) : "a"(number), "D"(a), "S"(b), "d"(c)
			 : "rcx", "r11", "memory");
	return result;
}

#define LINUX_WRITE 1
#define LINUX_CLOCK_GETTIME 228
#define LINUX_EXIT 231
#define MONOTONIC 1

void leave(int status)
{
	linux_call(LINUX_EXIT, status, 0, 0);
	__builtin_trap();
}

static _Alignas(4096) unsigned char arena[1 << 28];
static size_t arena_used;
size_t live_blocks;

void *stela_libc_allocate(size_t size, size_t alignment)
{
	size_t start = (arena_used + alignment - 1) & ~(alignment - 1);
	if (start + size > sizeof(arena))
		return NULL;
	arena_used = start + size;
	live_blocks++;
	return arena + start;
}

void stela_libc_release(void *pointer, size_t size, size_t alignment)
{
	(void)pointer;
	(void)size;
	(void)alignment;
	live_blocks--;
}

int logging;
char logged[4096];
size_t logged_bytes;

void stela_libc_log(const char *text, size_t size)
{
	if (!logging) {
		linux_call(LINUX_WRITE, 2, (long)text, (long)size);
		return;
	}
	for (size_t at = 0; at < size && logged_bytes + 1 < sizeof(logged); at++)
		logged[logged_bytes++] = text[at];
	logged[logged_bytes] = 0;
}

_Noreturn void stela_libc_fatal(const char *message)
{
	stela_libc_log(message, strlen(message));
	leave(2);
	__builtin_unreachable();
}

uint64_t stela_libc_memory_bytes(void)
{
	return 1ull << 32;
}

uint64_t stela_libc_available_memory_bytes(void)
{
	return 3ull << 30;
}

uint64_t stela_libc_now_ns(void)
{
	struct {
		long seconds, nanoseconds;
	} time;
	linux_call(LINUX_CLOCK_GETTIME, MONOTONIC, (long)&time, 0);
	return (uint64_t)time.seconds * 1000000000 + (uint64_t)time.nanoseconds;
}

unsigned int current_core;
int thread_start_error;

unsigned int stela_libc_core(void)
{
	return current_core;
}

/* The boot's default: Intel's CCS compression off. */
int stela_libc_intel_ccs(void)
{
	return 0;
}

/* One host processor: a thread runs through at its start. */
int stela_libc_thread_start(void (*body)(void *), void *argument, unsigned int core)
{
	if (thread_start_error)
		return thread_start_error;
	unsigned int caller = current_core;
	current_core = core;
	body(argument);
	current_core = caller;
	return 0;
}

unsigned int waits, wakes;
volatile unsigned int *waited_word;
unsigned int waited_value;

void stela_libc_wait(const volatile unsigned int *word, unsigned int value, uint64_t deadline)
{
	(void)deadline;
	waits++;
	if (word == waited_word)
		*waited_word = waited_value;
	(void)value;
}

void stela_libc_wake(const volatile unsigned int *word)
{
	(void)word;
	wakes++;
}

/* The render nodes a test lists, whose descriptors are 3 onwards in the
 * list's order, and how often a node was opened. */
struct stela_libc_drm_device drm_devices[2];
unsigned int drm_device_count, drm_opens;

int stela_libc_drm_open(unsigned int minor)
{
	(void)minor;
	drm_opens++;
	return -ENODEV;
}

int stela_libc_drm_close(int fd)
{
	(void)fd;
	return -ENODEV;
}

int stela_libc_drm_dup(int fd)
{
	(void)fd;
	return -ENODEV;
}

long stela_libc_drm_rdev(int fd)
{
	if (fd < 3 || (unsigned int)fd - 3 >= drm_device_count)
		return -ENODEV;
	/* Linux's dev_t: the major above twenty bits of minor. */
	return (226l << 20) | (128 + drm_devices[fd - 3].minor);
}

long long stela_libc_drm_seek(int fd, long long offset, int whence)
{
	(void)fd;
	(void)offset;
	(void)whence;
	return -ENODEV;
}

int stela_libc_drm_ioctl(int fd, unsigned long request, void *argument)
{
	(void)fd;
	(void)request;
	(void)argument;
	return -ENODEV;
}

void *stela_libc_drm_mmap(int fd, uint64_t offset, size_t size)
{
	(void)fd;
	(void)offset;
	(void)size;
	return (void *)-ENODEV;
}

/* One anonymous file, whose calls a test reads back. */
int file_mapped_fd;
void *file_mapped_address;
uint64_t file_bytes;

int stela_libc_file_create(void)
{
	return STELA_LIBC_FILE_BASE;
}

int stela_libc_file_close(int fd)
{
	return fd == STELA_LIBC_FILE_BASE ? 0 : -EBADF;
}

int stela_libc_file_resize(int fd, uint64_t size)
{
	file_bytes = size;
	return fd == STELA_LIBC_FILE_BASE ? 0 : -EBADF;
}

void *stela_libc_file_mmap(int fd, uint64_t offset, size_t size, void *address)
{
	(void)offset;
	(void)size;
	file_mapped_fd = fd;
	file_mapped_address = address;
	return address ? address : (void *)0x1000;
}

int stela_libc_file_munmap(void *address, size_t size)
{
	(void)size;
	return address == (void *)0x1000;
}

/* Inject the real host ABI's recognized-mapping result without freeing the
 * test's allocator backing, which lets the fixture detect an incorrect free. */
void *drm_unmap_address;
int drm_unmap_result;

int stela_libc_drm_munmap(void *address, size_t size)
{
	(void)size;
	return address == drm_unmap_address ? drm_unmap_result : 0;
}

unsigned int stela_libc_drm_devices(void)
{
	return drm_device_count;
}

void stela_libc_drm_device(unsigned int index, struct stela_libc_drm_device *device)
{
	*device = drm_devices[index];
}

__asm__(".globl _start\n_start:\n\tand $-16, %rsp\n\tcall start\n");

extern void (*__init_array_start[])(void);
extern void (*__init_array_end[])(void);

__attribute__((used)) static void start(void)
{
	for (void (**constructor)(void) = __init_array_start; constructor < __init_array_end;
	     constructor++)
		(*constructor)();
	leave(main());
}
