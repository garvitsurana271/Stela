// SPDX-License-Identifier: GPL-2.0-or-later
/* Linux's tick count. As x86's linker script does, jiffies names
 * jiffies_64's low word, which on LP64 is all of it. Service passes, resuming
 * tasks, clock reads and udelay() advance it (kernel/service.c); until the
 * first it holds INITIAL_JIFFIES. */
#include <linux/jiffies.h>

u64 jiffies_64 __cacheline_aligned_in_smp = INITIAL_JIFFIES;
extern unsigned long volatile jiffies __attribute__((alias("jiffies_64")));

#ifdef STELA_LINUX_JIFFIES_TEST
#include <linux/ktime.h>
#include <linux/ratelimit.h>
#include <linux/timekeeping.h>
#include <stela/linux.h>
#include <stela/test.h>

static unsigned int messages;
static s64 clock_ns;

/* A clock that moves a millisecond at each read, so a loop's reads alone
 * are what move it. */
static s64 stepping_clock(void *context)
{
	(void)context;
	return clock_ns += NSEC_PER_MSEC;
}

static void log_message(void *context, int level, const char *message)
{
	(void)context;
	assert(level == LOGLEVEL_WARNING && strstr(message, "callbacks suppressed"));
	messages++;
}

int main(void)
{
	assert(jiffies == INITIAL_JIFFIES && get_jiffies_64() == INITIAL_JIFFIES);
	WRITE_ONCE(jiffies_64, INITIAL_JIFFIES + 1);
	assert(jiffies == INITIAL_JIFFIES + 1);
	const struct stela_linux_services services = {.log = log_message};
	const struct stela_linux_owner owner = {.services = &services};
	stela_linux_enter(&owner);
	/* Two calls per tenth of a second; the tick moves the window. */
	DEFINE_RATELIMIT_STATE(state, HZ / 10, 2);
	assert(__ratelimit(&state) && __ratelimit(&state));
	assert(!__ratelimit(&state) && !__ratelimit(&state));
	WRITE_ONCE(jiffies_64, jiffies_64 + HZ / 10 - 1);
	assert(!__ratelimit(&state) && !messages);
	WRITE_ONCE(jiffies_64, jiffies_64 + 2);
	assert(__ratelimit(&state));
	/* The original warning is deferred while the rate-limit lock is held. */
	stela_linux_printk_flush();
	assert(messages == 1);
	assert(__ratelimit(&state) && !__ratelimit(&state));
	stela_linux_exit();

	/* Code that spins on a jiffies bound reading the clock, and blocks for
	 * nothing, sees jiffies follow the clock: nothing else moves them while
	 * it spins. The loop's only way out is jiffies passing the bound. */
	const struct stela_linux_services clocked = {.now_ns = stepping_clock};
	const struct stela_linux_owner timed = {.services = &clocked};
	stela_linux_enter(&timed);
	unsigned long end = jiffies + HZ / 10;
	unsigned int reads = 0;
	while (time_before_eq(jiffies, end)) {
		(void)ktime_get();
		assert(++reads < 100000);
	}
	/* Never backwards: an earlier sample leaves them as they are. */
	unsigned long reached = jiffies;
	clock_ns = 0;
	(void)ktime_get();
	assert(jiffies == reached);
	stela_linux_exit();
	printf("Linux jiffies: one tick word under both names, tick-driven rate limits and jiffies that follow a spinning clock passed\n");
	return 0;
}
#endif
