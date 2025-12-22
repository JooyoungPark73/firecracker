// SPDX-License-Identifier: GPL-2.0
/*
 * Khala PCI Shared Memory Driver (Benchmark Edition)
 *
 * Capabilities:
 * 1. /dev/khalaX read/write: Zero-Copy, Uncached (Write-Combining) -> Low Latency.
 * 2. /dev/khalaX mmap: Zero-Copy, Cached (Write-Back) -> High Throughput.
 * 3. Restore-Aware: Automatically warms up EPT on snapshot restore.
 */

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/pci.h>
#include <linux/fs.h>
#include <linux/cdev.h>
#include <linux/uaccess.h>
#include <linux/io.h>
#include <linux/idr.h>
#include <linux/list.h>
#include <linux/spinlock.h>
#include <linux/acpi.h>
#include <acpi/acpi_bus.h>
#include <linux/mm.h>

#define DRIVER_NAME "khala"
#define MAX_DEVICES 32
#define VMGENID_HID "FCVMGID"

static dev_t khala_base_dev;
static struct class *khala_class;
static DEFINE_IDA(khala_dev_ids);
static LIST_HEAD(khala_dev_list);
static DEFINE_SPINLOCK(khala_dev_lock);
static acpi_handle vmgenid_handle = NULL;

struct khala_dev {
    struct pci_dev *pdev;
    void __iomem *bar_base;     /* WC Mapping for read/write */
    resource_size_t bar_phys;   /* Physical Address for mmap */
    resource_size_t bar_len;
    struct cdev cdev;
    int id;
    struct list_head node;
};

/* --- ACPI Restore Handler --- */
static void khala_acpi_notify(acpi_handle handle, u32 event, void *data)
{
    struct khala_dev *dev;
    unsigned long offset;
    volatile u8 dummy;

    if (event != 0x80) return;

    rcu_read_lock();
    list_for_each_entry_rcu(dev, &khala_dev_list, node) {
        pr_info("khala%d: Restore Detected! Warming up...\n", dev->id);
        for (offset = 0; offset < dev->bar_len; offset += 4096) {
            dummy = ioread8(dev->bar_base + offset);
        }
    }
    rcu_read_unlock();
}

/* --- File Operations --- */

static int khala_open(struct inode *inode, struct file *filp)
{
    struct khala_dev *dev = container_of(inode->i_cdev, struct khala_dev, cdev);
    filp->private_data = dev;
    return 0;
}

/* * READ/WRITE: Uses ioremap_wc (Uncached/Write-Combining).
 * Best for: Single-shot latency, writing to host.
 */
static ssize_t khala_read(struct file *filp, char __user *buf, 
                          size_t count, loff_t *f_pos)
{
    struct khala_dev *dev = filp->private_data;
    size_t remain = dev->bar_len - *f_pos;
    size_t to_copy = min(count, remain);

    if (*f_pos >= dev->bar_len) return 0;
    if (copy_to_user(buf, dev->bar_base + *f_pos, to_copy)) return -EFAULT;

    *f_pos += to_copy;
    return to_copy;
}

static ssize_t khala_write(struct file *filp, const char __user *buf, 
                           size_t count, loff_t *f_pos)
{
    struct khala_dev *dev = filp->private_data;
    size_t remain = dev->bar_len - *f_pos;
    size_t to_copy = min(count, remain);

    if (*f_pos >= dev->bar_len) return -ENOSPC;
    if (copy_from_user(dev->bar_base + *f_pos, buf, to_copy)) return -EFAULT;

    *f_pos += to_copy;
    return to_copy;
}

/*
 * MMAP: Uses remap_pfn_range (Cached/Write-Back).
 * Best for: Throughput, repeated access, random access.
 */
static int khala_mmap(struct file *filp, struct vm_area_struct *vma)
{
    struct khala_dev *dev = filp->private_data;
    unsigned long size = vma->vm_end - vma->vm_start;
    unsigned long pfn;
    int ret;

    /* Check bounds against PCI BAR size */
    if (size > dev->bar_len) {
        pr_err("khala%d: mmap request exceeds BAR size\n", dev->id);
        return -EINVAL;
    }

    /* Calculate PFN relative to the BAR start + offset requested in mmap */
    pfn = (dev->bar_phys >> PAGE_SHIFT) + vma->vm_pgoff;

    /* * OPTIMIZATION FLAGS:
     * VM_IO | VM_PFNMAP: Tells kernel this is non-system RAM (IO Memory).
     * VM_DONTEXPAND | VM_DONTDUMP: Standard for driver mappings.
     * Note: We do NOT set VM_MIXEDMAP so we can use pure PFN mapping.
     */
    vma->vm_flags |= VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP;

    ret = remap_pfn_range(vma, vma->vm_start, pfn, size, vma->vm_page_prot);
    if (ret) {
        pr_err("khala%d: remap_pfn_range failed: %d\n", dev->id, ret);
        return ret;
    }

    return 0;
}

static loff_t khala_llseek(struct file *filp, loff_t offset, int whence)
{
    struct khala_dev *dev = filp->private_data;
    loff_t newpos;
    switch (whence) {
        case SEEK_SET: newpos = offset; break;
        case SEEK_CUR: newpos = filp->f_pos + offset; break;
        case SEEK_END: newpos = dev->bar_len + offset; break;
        default: return -EINVAL;
    }
    if (newpos < 0 || newpos > dev->bar_len) return -EINVAL;
    filp->f_pos = newpos;
    return newpos;
}

static const struct file_operations khala_fops = {
    .owner = THIS_MODULE,
    .open = khala_open,
    .read = khala_read,
    .write = khala_write,
    .mmap = khala_mmap,
    .llseek = khala_llseek,
};

/* --- PCI Lifecycle --- */

static int khala_probe(struct pci_dev *pdev, const struct pci_device_id *id)
{
    int ret;
    struct khala_dev *dev;
    struct device *device_node;

    ret = pci_enable_device(pdev);
    if (ret) return ret;

    ret = pci_request_regions(pdev, DRIVER_NAME);
    if (ret) goto err_disable;

    dev = kzalloc(sizeof(*dev), GFP_KERNEL);
    if (!dev) {
        ret = -ENOMEM;
        goto err_regions;
    }
    dev->pdev = pdev;
    dev->id = ida_alloc(&khala_dev_ids, GFP_KERNEL);
    if (dev->id < 0) {
        ret = dev->id;
        goto err_kfree;
    }

    /* Grab Physical info for mmap */
    dev->bar_phys = pci_resource_start(pdev, 0);
    dev->bar_len = pci_resource_len(pdev, 0);

    /* Grab Virtual info for read/write (Write-Combining) */
    dev->bar_base = devm_ioremap_wc(&pdev->dev, dev->bar_phys, dev->bar_len);
    if (!dev->bar_base) {
        ret = -ENOMEM;
        goto err_ida;
    }

    /* WARMUP */
    {
        unsigned long off; 
        volatile u8 d;
        pr_info("khala%d: Boot-time warmup (%llu MB)...\n", dev->id, dev->bar_len >> 20);
        for(off = 0; off < dev->bar_len; off += 4096) d = ioread8(dev->bar_base + off);
    }

    /* Register */
    spin_lock(&khala_dev_lock);
    list_add_rcu(&dev->node, &khala_dev_list);
    spin_unlock(&khala_dev_lock);

    cdev_init(&dev->cdev, &khala_fops);
    dev->cdev.owner = THIS_MODULE;
    ret = cdev_add(&dev->cdev, khala_base_dev + dev->id, 1);
    if (ret) goto err_list_del;

    device_node = device_create(khala_class, &pdev->dev, 
                                khala_base_dev + dev->id, NULL, "khala%d", dev->id);
    if (IS_ERR(device_node)) {
        ret = PTR_ERR(device_node);
        goto err_cdev;
    }

    pci_set_drvdata(pdev, dev);
    pr_info("khala: Device /dev/khala%d attached.\n", dev->id);
    return 0;

err_cdev:
    cdev_del(&dev->cdev);
err_list_del:
    spin_lock(&khala_dev_lock);
    list_del_rcu(&dev->node);
    spin_unlock(&khala_dev_lock);
err_ida:
    ida_free(&khala_dev_ids, dev->id);
err_kfree:
    kfree(dev);
err_regions:
    pci_release_regions(pdev);
err_disable:
    pci_disable_device(pdev);
    return ret;
}

static void khala_remove(struct pci_dev *pdev)
{
    struct khala_dev *dev = pci_get_drvdata(pdev);
    spin_lock(&khala_dev_lock);
    list_del_rcu(&dev->node);
    spin_unlock(&khala_dev_lock);
    synchronize_rcu();

    device_destroy(khala_class, khala_base_dev + dev->id);
    cdev_del(&dev->cdev);
    ida_free(&khala_dev_ids, dev->id);
    pci_release_regions(pdev);
    pci_disable_device(pdev);
    kfree(dev);
}

/* --- Init/Exit --- */

static const struct pci_device_id khala_ids[] = { { PCI_DEVICE(0x1234, 0x1110) }, { 0, } };
MODULE_DEVICE_TABLE(pci, khala_ids);
static struct pci_driver khala_driver = {
    .name = DRIVER_NAME,
    .id_table = khala_ids,
    .probe = khala_probe,
    .remove = khala_remove,
};

static acpi_status khala_find_vmgenid_cb(acpi_handle handle, u32 lvl, void *context, void **rv)
{
    struct acpi_device_info *info;
    if (ACPI_FAILURE(acpi_get_object_info(handle, &info))) return AE_OK;
    if (info->valid & ACPI_VALID_HID && !strcmp(info->hardware_id.string, VMGENID_HID)) {
        pr_info("khala: Found VMGenID. Hooking restore events.\n");
        vmgenid_handle = handle;
        acpi_install_notify_handler(handle, ACPI_DEVICE_NOTIFY, khala_acpi_notify, NULL);
    }
    kfree(info);
    return AE_OK;
}

static int __init khala_init(void)
{
    int ret = alloc_chrdev_region(&khala_base_dev, 0, MAX_DEVICES, DRIVER_NAME);
    if (ret) return ret;
    khala_class = class_create(THIS_MODULE, DRIVER_NAME);
    
    /* Hook ACPI */
    acpi_walk_namespace(ACPI_TYPE_DEVICE, ACPI_ROOT_OBJECT, ACPI_UINT32_MAX, 
                        khala_find_vmgenid_cb, NULL, NULL, NULL);
    return pci_register_driver(&khala_driver);
}

static void __exit khala_exit(void)
{
    if (vmgenid_handle) acpi_remove_notify_handler(vmgenid_handle, ACPI_DEVICE_NOTIFY, khala_acpi_notify);
    pci_unregister_driver(&khala_driver);
    class_destroy(khala_class);
    unregister_chrdev_region(khala_base_dev, MAX_DEVICES);
    ida_destroy(&khala_dev_ids);
}

module_init(khala_init);
module_exit(khala_exit);
MODULE_LICENSE("GPL");