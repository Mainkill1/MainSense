// SPDX-License-Identifier: GPL-2.0-only OR MIT
/* Read named SoC and graphics voltages from the Strix Halo SMU table. */

#include <linux/delay.h>
#include <linux/hwmon.h>
#include <linux/init.h>
#include <linux/io.h>
#include <linux/jiffies.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/pci.h>
#include <linux/suspend.h>
#include <asm/processor.h>

#if __has_include(<asm/amd/node.h>)
#include <asm/amd/node.h>
#else
#include <asm/amd_nb.h>
#endif

#define RSMU_MESSAGE	0x03b10a20
#define RSMU_RESPONSE	0x03b10a80
#define RSMU_ARGUMENT	0x03b10a88
#define RSMU_ARGUMENTS	6
#define RSMU_GET_TABLE	0x06
#define RSMU_TABLE_BASE	0x66
#define RSMU_REFRESH	0x65
#define RSMU_SUCCESS	0x01
#define RSMU_BUSY	0xfc
#define RSMU_REJECTED	0xfd

#define TABLE_REVISION	0x64020c
#define TABLE_BYTES	0x1028
#define SOC_VOLTAGE_OFFSET	0x338	/* Saku Overclock ResolveSocVoltage(), index 206 */
#define GFX_VOLTAGE_OFFSET	0x54c	/* RyzenAdj get_gfx_volt(), table 0x64020c */
#define MIN_UPDATE_MS	250

static const struct {
	unsigned int offset;
	const char *label;
} voltage_fields[] = {
	/* Keep the existing graphics channel at in0 for log readers. */
	{ GFX_VOLTAGE_OFFSET, "GFX voltage (SMU)" },
	{ SOC_VOLTAGE_OFFSET, "SoC voltage (SMU)" },
};

static struct pci_dev *host;
static struct device *hwmon;
static void __iomem *voltage_window;
static DEFINE_MUTEX(telemetry_lock);
static unsigned long next_update;
static bool cached, suspended;
static int read_status, millivolts[ARRAY_SIZE(voltage_fields)];

static int wait_for_response(u32 *response)
{
	int attempt, ret;

	for (attempt = 0; attempt < 100; attempt++) {
		ret = amd_smn_read(0, RSMU_RESPONSE, response);
		if (ret || *response)
			return ret;
		usleep_range(100, 200);
	}
	return -ETIMEDOUT;
}

/* Only the table identification, address and refresh requests call this. */
static int telemetry_request(u32 message, u32 arg0, u32 arg1, u32 *result)
{
	const u32 args[RSMU_ARGUMENTS] = { arg0, arg1 };
	u32 response;
	int ret, i;

	ret = wait_for_response(&response);
	if (ret)
		return ret;
	for (i = 0; i < ARRAY_SIZE(args); i++) {
		ret = amd_smn_write(0, RSMU_ARGUMENT + i * sizeof(u32), args[i]);
		if (ret)
			return ret;
	}
	ret = amd_smn_write(0, RSMU_RESPONSE, 0);
	if (ret)
		return ret;
	ret = amd_smn_write(0, RSMU_MESSAGE, message);
	if (ret)
		return ret;
	ret = wait_for_response(&response);
	if (ret)
		return ret;
	if (response == RSMU_BUSY || response == RSMU_REJECTED)
		return -EAGAIN;
	if (response != RSMU_SUCCESS)
		return -EIO;
	if (!result)
		return 0;
	ret = amd_smn_read(0, RSMU_ARGUMENT, &result[0]);
	if (ret)
		return ret;
	return amd_smn_read(0, RSMU_ARGUMENT + sizeof(u32), &result[1]);
}

static int map_voltage(void)
{
	u32 response[2];
	u64 address, address_limit;
	int ret;

	ret = telemetry_request(RSMU_GET_TABLE, 0, 0, response);
	if (ret)
		return ret;
	if (response[0] != TABLE_REVISION || response[1] != TABLE_BYTES) {
		pr_info("mainsense_strix_halo: unsupported SMU table %#x (%u bytes)\n",
			response[0], response[1]);
		return -EOPNOTSUPP;
	}

	ret = telemetry_request(RSMU_TABLE_BASE, 1, 1, response);
	if (ret)
		return ret;
	address = ((u64)response[1] << 32) | response[0];
	address_limit = BIT_ULL(boot_cpu_data.x86_phys_bits);
	if (address < PAGE_SIZE || !IS_ALIGNED(address, sizeof(u32)) ||
	    address > address_limit - TABLE_BYTES)
		return -EIO;
	voltage_window = ioremap_cache(address + SOC_VOLTAGE_OFFSET,
				      GFX_VOLTAGE_OFFSET - SOC_VOLTAGE_OFFSET + sizeof(u32));
	return voltage_window ? 0 : -ENOMEM;
}

/* Convert the firmware's binary32 volts without using the kernel FPU. */
static int voltage_millivolts(u32 word)
{
	u32 exponent = (word >> 23) & 0xff;
	u64 mantissa, value;
	unsigned int shift;

	if ((word & BIT(31)) || exponent < 115 || exponent > 128)
		return -ENODATA;
	shift = 150 - exponent;
	mantissa = (word & GENMASK(22, 0)) | BIT(23);
	value = (mantissa * 1000 + BIT_ULL(shift - 1)) >> shift;
	return value && value <= 2500 ? (int)value : -ENODATA;
}

static int refresh_voltages(void)
{
	int ret, i;

	if (!voltage_window) {
		ret = map_voltage();
		if (ret)
			return ret;
	}
	ret = telemetry_request(RSMU_REFRESH, 3, 0, NULL);
	if (ret)
		return ret;
	for (i = 0; i < ARRAY_SIZE(voltage_fields); i++) {
		unsigned int offset = voltage_fields[i].offset - SOC_VOLTAGE_OFFSET;

		millivolts[i] = voltage_millivolts(readl(voltage_window + offset));
	}
	return 0;
}

static umode_t voltage_visible(const void *data, enum hwmon_sensor_types type,
			       u32 attr, int channel)
{
	return 0444;
}

static int voltage_read(struct device *dev, enum hwmon_sensor_types type, u32 attr,
			int channel, long *value)
{
	int ret;

	mutex_lock(&telemetry_lock);
	if (suspended) {
		ret = -EAGAIN;
		goto out;
	}
	if (!cached || time_after_eq(jiffies, next_update)) {
		read_status = refresh_voltages();
		next_update = jiffies + msecs_to_jiffies(MIN_UPDATE_MS);
		cached = true;
	}
	ret = read_status ?: millivolts[channel];
	if (ret >= 0) {
		*value = ret;
		ret = 0;
	}
out:
	mutex_unlock(&telemetry_lock);
	return ret;
}

static int voltage_label(struct device *dev, enum hwmon_sensor_types type, u32 attr,
			 int channel, const char **label)
{
	*label = voltage_fields[channel].label;
	return 0;
}

static const struct hwmon_ops voltage_ops = {
	.is_visible = voltage_visible,
	.read = voltage_read,
	.read_string = voltage_label,
};

static const struct hwmon_channel_info *voltage_channels[] = {
	HWMON_CHANNEL_INFO(in, HWMON_I_INPUT | HWMON_I_LABEL,
			   HWMON_I_INPUT | HWMON_I_LABEL),
	NULL,
};

static const struct hwmon_chip_info voltage_chip = {
	.ops = &voltage_ops,
	.info = voltage_channels,
};

static int voltage_pm_event(struct notifier_block *notifier, unsigned long action, void *unused)
{
	mutex_lock(&telemetry_lock);
	switch (action) {
	case PM_SUSPEND_PREPARE:
	case PM_HIBERNATION_PREPARE:
	case PM_RESTORE_PREPARE:
		suspended = true;
		if (voltage_window)
			iounmap(voltage_window);
		voltage_window = NULL;
		cached = false;
		break;
	case PM_POST_SUSPEND:
	case PM_POST_HIBERNATION:
	case PM_POST_RESTORE:
		suspended = false;
		break;
	}
	mutex_unlock(&telemetry_lock);
	return NOTIFY_OK;
}

static struct notifier_block voltage_pm_notifier = {
	.notifier_call = voltage_pm_event,
};

static int __init voltage_init(void)
{
	struct pci_dev *gpu;
	int ret, attempt;

	if (boot_cpu_data.x86_vendor != X86_VENDOR_AMD ||
	    boot_cpu_data.x86 != 0x1a || boot_cpu_data.x86_model != 0x70)
		return -ENODEV;
	gpu = pci_get_device(PCI_VENDOR_ID_ATI, 0x1586, NULL);
	if (!gpu)
		return -ENODEV;
	pci_dev_put(gpu);
	host = pci_get_domain_bus_and_slot(0, 0, PCI_DEVFN(0, 0));
	if (!host)
		return -ENODEV;
	if (host->vendor != PCI_VENDOR_ID_AMD || host->device != 0x1507) {
		ret = -ENODEV;
		goto put_host;
	}
	ret = register_pm_notifier(&voltage_pm_notifier);
	if (ret)
		goto put_host;

	for (attempt = 0; attempt < 3; attempt++) {
		mutex_lock(&telemetry_lock);
		ret = suspended ? -EBUSY : map_voltage();
		mutex_unlock(&telemetry_lock);
		if (ret != -EAGAIN)
			break;
		msleep(MIN_UPDATE_MS);
	}
	if (ret)
		goto unregister_notifier;

	hwmon = hwmon_device_register_with_info(&host->dev, "strixhalo_smu", NULL,
					       &voltage_chip, NULL);
	if (!IS_ERR(hwmon))
		return 0;
	ret = PTR_ERR(hwmon);

unregister_notifier:
	unregister_pm_notifier(&voltage_pm_notifier);
	if (voltage_window)
		iounmap(voltage_window);
put_host:
	pci_dev_put(host);
	return ret;
}

static void __exit voltage_exit(void)
{
	hwmon_device_unregister(hwmon);
	unregister_pm_notifier(&voltage_pm_notifier);
	if (voltage_window)
		iounmap(voltage_window);
	pci_dev_put(host);
}

module_init(voltage_init);
module_exit(voltage_exit);
MODULE_DESCRIPTION("Read-only hwmon SoC and graphics voltages from the Strix Halo SMU table");
MODULE_LICENSE("Dual MIT/GPL");
