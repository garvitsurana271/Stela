// SPDX-License-Identifier: GPL-2.0-only
/* The x86 processor services Linux code here reads (asm/cpufeature.h,
 * asm/tsc.h, asm/smp.h), for the processor Linux runs on. */
#include <linux/bits.h>
#include <linux/compiler.h>
#include <linux/cpumask.h>
#include <linux/irqflags.h>
#include <linux/preempt.h>
#include <linux/smp.h>
#include <linux/types.h>
#include <asm/cpufeature.h>
#include <asm/smp.h>
#include <asm/tsc.h>
#include <stela/linux.h>

/* The owner's calibrated TSC rate in kHz, set when the environment starts
 * (task_bootstrap.c), as x86 sets it at its TSC's calibration. */
unsigned int tsc_khz;

static void cpuid(u32 leaf, u32 subleaf, u32 *eax, u32 *ebx, u32 *ecx, u32 *edx)
{
	asm volatile("cpuid"
		     : "=a"(*eax), "=b"(*ebx), "=c"(*ecx), "=d"(*edx)
		     : "a"(leaf), "c"(subleaf));
}

bool stela_linux_cpu_has(unsigned int feature)
{
	u32 eax, ebx, ecx, edx, word;

	switch (feature / 32) {
	case 0:
		cpuid(1, 0, &eax, &ebx, &ecx, &edx);
		word = edx;
		break;
	case 4:
		cpuid(1, 0, &eax, &ebx, &ecx, &edx);
		word = ecx;
		break;
	case 9:
		cpuid(0, 0, &eax, &ebx, &ecx, &edx);
		if (eax < 7)
			return false;
		cpuid(7, 0, &eax, &ebx, &ecx, &edx);
		word = ebx;
		break;
	default:
		/* asm/cpufeature.h names no feature of another word. */
		return false;
	}
	return (word & BIT(feature % 32)) != 0;
}

/* kernel/up.c's: Linux code here knows one processor (NR_CPUS 1), so the
 * function runs on it when the condition and the mask name it. */
void on_each_cpu_cond_mask(smp_cond_func_t cond_func, smp_call_func_t func, void *info, bool wait,
			   const struct cpumask *mask)
{
	unsigned long flags;

	(void)wait;
	preempt_disable();
	if ((!cond_func || cond_func(0, info)) && cpumask_test_cpu(0, mask)) {
		local_irq_save(flags);
		func(info);
		local_irq_restore(flags);
	}
	preempt_enable();
}

/* x86 runs WBINVD on every processor. Linux code here runs on one processor
 * and has no call onto the others, so it writes back and invalidates this
 * processor's caches, which on x86 also covers the shared last-level cache,
 * and says once that the other processors' private caches are not. Its
 * callers are i915's GGTT resume and a dma-buf import on GPUs without a
 * shared last-level cache. */
void wbinvd_on_all_cpus(void)
{
	asm volatile("wbinvd" ::: "memory");
	stela_linux_unsupported("writing back the other processors' private caches: only this processor's caches were written back");
}
