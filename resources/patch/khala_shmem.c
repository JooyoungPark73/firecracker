// SPDX-License-Identifier: GPL-2.0
/*
 * Khala Shared Memory Character Device Driver
 *
 * This driver exposes a physical memory region reserved by Firecracker
 * as a character device (/dev/khala-shmem) that can be mmap'd by userspace
 * applications for host-guest communication.
 *
 * The physical address and size are passed via kernel command line:
 *   khala_shmem=<phys_addr>,<size>
 *
 * Example: khala_shmem=0x100000000,0x1000000 (256MB at 4GB)
 * Changes:
 * - Switched to remap_pfn_range for manual control (No VM_IO flag).
 * - Added VM_MIXEDMAP to support raw PFN mapping without forced IO side-effects.
 * - Enforces Write-Back caching via default protection + no VM_IO.
 */

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/fs.h>
#include <linux/cdev.h>
#include <linux/device.h>
#include <linux/mm.h>
#include <linux/io.h>
#include <linux/uaccess.h>
/* Needed for version checks if compiling on Kernel 6.4+ */
#include <linux/version.h> 

#define DEVICE_NAME "khala-shmem"
#define CLASS_NAME "khala"

static dev_t khala_dev_num;
static struct cdev khala_cdev;
static struct class *khala_class;
static struct device *khala_device;

static phys_addr_t shmem_phys_addr;
static size_t shmem_size;

/* Parse khala_shmem=<addr>,<size> */
static int __init parse_khala_shmem(char *str)
{
	char *endp;

	if (!str) return 0;

	shmem_phys_addr = simple_strtoull(str, &endp, 0);
	if (*endp != ',') return 0;

	str = endp + 1;
	shmem_size = simple_strtoull(str, &endp, 0);

	pr_info("khala_shmem: cmdline parsed: addr=0x%llx size=0x%zx\n",
		(unsigned long long)shmem_phys_addr, shmem_size);

	return 1;
}
__setup("khala_shmem=", parse_khala_shmem);

static int khala_open(struct inode *inode, struct file *filp)
{
	return 0;
}

static int khala_release(struct inode *inode, struct file *filp)
{
	return 0;
}

static loff_t khala_llseek(struct file *filp, loff_t offset, int whence)
{
	loff_t newpos;

	switch (whence) {
	case SEEK_SET: newpos = offset; break;
	case SEEK_CUR: newpos = filp->f_pos + offset; break;
	case SEEK_END: newpos = shmem_size + offset; break;
	default: return -EINVAL;
	}

	if (newpos < 0 || newpos > shmem_size) return -EINVAL;
	filp->f_pos = newpos;
	return newpos;
}

static int khala_mmap(struct file *filp, struct vm_area_struct *vma)
{
	unsigned long size = vma->vm_end - vma->vm_start;
	unsigned long offset = vma->vm_pgoff << PAGE_SHIFT;
	unsigned long pfn;
	int ret;

	/* Check bounds */
	if (offset + size > shmem_size) {
		pr_err("khala_shmem: mmap request exceeds region size\n");
		return -EINVAL;
	}

	/* Calculate PFN (Physical Frame Number) */
	pfn = (shmem_phys_addr + offset) >> PAGE_SHIFT;

	/* * OPTIMIZATION: 
	 * 1. VM_IO is NOT set (vm_iomap_memory would set it).
	 * This ensures the CPU treats it as memory (cacheable), not MMIO.
	 * 2. VM_MIXEDMAP allows raw PFNs to live in the VMA without struct pages.
	 * This is friendlier to future Huge Page optimizations.
	 */
	vma->vm_flags |= VM_DONTEXPAND | VM_DONTDUMP | VM_MIXEDMAP;

	/* * Use remap_pfn_range instead of vm_iomap_memory.
	 * We trust vma->vm_page_prot (defaults to Write-Back) is correct.
	 */
	ret = remap_pfn_range(vma, vma->vm_start, pfn, size, vma->vm_page_prot);
	if (ret) {
		pr_err("khala_shmem: remap_pfn_range failed: %d\n", ret);
		return ret;
	}

	pr_debug("khala_shmem: mmap success: virt=0x%lx pfn=0x%lx size=0x%lx\n",
		 vma->vm_start, pfn, size);

	return 0;
}

static const struct file_operations khala_fops = {
	.owner = THIS_MODULE,
	.open = khala_open,
	.release = khala_release,
	.mmap = khala_mmap,
	.llseek = khala_llseek,
};

static int __init khala_shmem_init(void)
{
	int ret;

	if (!shmem_phys_addr || !shmem_size) {
		pr_info("khala_shmem: missing khala_shmem= cmdline\n");
		return -ENODEV;
	}

	/* Check Alignment */
	if (!PAGE_ALIGNED(shmem_phys_addr) || !PAGE_ALIGNED(shmem_size)) {
		pr_err("khala_shmem: Address/Size must be page aligned\n");
		return -EINVAL;
	}

	ret = alloc_chrdev_region(&khala_dev_num, 0, 1, DEVICE_NAME);
	if (ret < 0) return ret;

	cdev_init(&khala_cdev, &khala_fops);
	khala_cdev.owner = THIS_MODULE;

	ret = cdev_add(&khala_cdev, khala_dev_num, 1);
	if (ret < 0) goto fail_cdev_add;

	/* Handle Kernel 6.4+ class_create API change */
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 4, 0)
	khala_class = class_create(CLASS_NAME);
#else
	khala_class = class_create(THIS_MODULE, CLASS_NAME);
#endif

	if (IS_ERR(khala_class)) {
		ret = PTR_ERR(khala_class);
		goto fail_class_create;
	}

	khala_device = device_create(khala_class, NULL, khala_dev_num, NULL, DEVICE_NAME);
	if (IS_ERR(khala_device)) {
		ret = PTR_ERR(khala_device);
		goto fail_device_create;
	}

	pr_info("khala_shmem: Initialized at phys=0x%llx size=0x%zx\n",
		(unsigned long long)shmem_phys_addr, shmem_size);

	return 0;

fail_device_create:
	class_destroy(khala_class);
fail_class_create:
	cdev_del(&khala_cdev);
fail_cdev_add:
	unregister_chrdev_region(khala_dev_num, 1);
	return ret;
}

static void __exit khala_shmem_exit(void)
{
	device_destroy(khala_class, khala_dev_num);
	class_destroy(khala_class);
	cdev_del(&khala_cdev);
	unregister_chrdev_region(khala_dev_num, 1);
	pr_info("khala_shmem: Unloaded\n");
}

module_init(khala_shmem_init);
module_exit(khala_shmem_exit);

MODULE_LICENSE("GPL");
MODULE_AUTHOR("Hyscale Lab NTUsg");
MODULE_DESCRIPTION("Khala shared memory character device driver");
MODULE_VERSION("1.1");