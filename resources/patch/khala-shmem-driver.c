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

#define DEVICE_NAME "khala-shmem"
#define CLASS_NAME "khala"

static dev_t khala_dev_num;
static struct cdev khala_cdev;
static struct class *khala_class;
static struct device *khala_device;

/* Shared memory region info from kernel cmdline */
static phys_addr_t shmem_phys_addr;
static size_t shmem_size;

/* Parse khala_shmem=<addr>,<size> from kernel cmdline */
static int __init parse_khala_shmem(char *str)
{
	char *endp;

	if (!str)
		return 0;

	shmem_phys_addr = simple_strtoull(str, &endp, 0);
	if (*endp != ',')
		return 0;

	str = endp + 1;
	shmem_size = simple_strtoull(str, &endp, 0);

	pr_info("khala_shmem: cmdline parsed: addr=0x%llx size=0x%zx\n",
		(unsigned long long)shmem_phys_addr, shmem_size);

	return 1;
}
__setup("khala_shmem=", parse_khala_shmem);

static int khala_open(struct inode *inode, struct file *filp)
{
	/* Allow multiple opens */
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
	case SEEK_SET:
		newpos = offset;
		break;
	case SEEK_CUR:
		newpos = filp->f_pos + offset;
		break;
	case SEEK_END:
		newpos = shmem_size + offset;
		break;
	default:
		return -EINVAL;
	}

	if (newpos < 0 || newpos > shmem_size)
		return -EINVAL;

	filp->f_pos = newpos;
	return newpos;
}

static int khala_mmap(struct file *filp, struct vm_area_struct *vma)
{
	unsigned long size = vma->vm_end - vma->vm_start;
	unsigned long offset = vma->vm_pgoff << PAGE_SHIFT;
	phys_addr_t phys;

	/* Check bounds */
	if (offset + size > shmem_size) {
		pr_err("khala_shmem: mmap request exceeds region size\n");
		return -EINVAL;
	}

	phys = shmem_phys_addr + offset;

	/* Ensure physical address is page-aligned */
	if (phys & ~PAGE_MASK) {
		pr_err("khala_shmem: physical address not page-aligned\n");
		return -EINVAL;
	}

	/*
	 * Use write-combining for better performance
	 * This allows writes to be buffered and improves throughput
	 * Alternative: pgprot_noncached() for strict ordering
	 */
	vma->vm_page_prot = pgprot_writecombine(vma->vm_page_prot);

	/* Prevent swapping and dumping of this memory */
	vma->vm_flags |= VM_IO | VM_DONTEXPAND | VM_DONTDUMP;

	/* Map the physical memory region */
	if (remap_pfn_range(vma, vma->vm_start,
			    phys >> PAGE_SHIFT,
			    size,
			    vma->vm_page_prot)) {
		pr_err("khala_shmem: remap_pfn_range failed\n");
		return -EAGAIN;
	}

	pr_debug("khala_shmem: mmap success: virt=0x%lx phys=0x%llx size=0x%lx\n",
		 vma->vm_start, (unsigned long long)phys, size);

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

	/* Check if shared memory was configured */
	if (!shmem_phys_addr || !shmem_size) {
		pr_info("khala_shmem: No shared memory configured (missing khala_shmem= cmdline)\n");
		return -ENODEV;
	}

	/* Validate alignment */
	if (!PAGE_ALIGNED(shmem_phys_addr) || !PAGE_ALIGNED(shmem_size)) {
		pr_err("khala_shmem: Physical address or size not page-aligned\n");
		return -EINVAL;
	}

	/* Allocate character device number */
	ret = alloc_chrdev_region(&khala_dev_num, 0, 1, DEVICE_NAME);
	if (ret < 0) {
		pr_err("khala_shmem: Failed to allocate device number: %d\n", ret);
		return ret;
	}

	/* Initialize character device */
	cdev_init(&khala_cdev, &khala_fops);
	khala_cdev.owner = THIS_MODULE;

	ret = cdev_add(&khala_cdev, khala_dev_num, 1);
	if (ret < 0) {
		pr_err("khala_shmem: Failed to add character device: %d\n", ret);
		goto fail_cdev_add;
	}

	/* Create device class */
	khala_class = class_create(THIS_MODULE, CLASS_NAME);
	if (IS_ERR(khala_class)) {
		ret = PTR_ERR(khala_class);
		pr_err("khala_shmem: Failed to create device class: %d\n", ret);
		goto fail_class_create;
	}

	/* Create device node */
	khala_device = device_create(khala_class, NULL, khala_dev_num,
				     NULL, DEVICE_NAME);
	if (IS_ERR(khala_device)) {
		ret = PTR_ERR(khala_device);
		pr_err("khala_shmem: Failed to create device: %d\n", ret);
		goto fail_device_create;
	}

	pr_info("khala_shmem: Initialized at phys=0x%llx size=0x%zx (/dev/%s)\n",
		(unsigned long long)shmem_phys_addr, shmem_size, DEVICE_NAME);

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
MODULE_VERSION("1.0");

