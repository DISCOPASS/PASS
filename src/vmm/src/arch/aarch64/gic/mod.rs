// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

mod gicv2;
mod gicv3;
mod regs;

use std::boxed::Box;
use std::result;

use gicv2::GICv2;
use gicv3::GICv3;
use kvm_ioctls::{DeviceFd, VmFd};
pub use regs::GicState;

use super::layout;

/// Errors thrown while setting up the GIC.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error while calling KVM ioctl for setting up the global interrupt controller.
    #[error("Error while calling KVM ioctl for setting up the global interrupt controller: {0}")]
    CreateGIC(kvm_ioctls::Error),
    /// Error while setting or getting device attributes for the GIC.
    #[error("Error while setting or getting device attributes for the GIC: {0}, {1}, {2}")]
    DeviceAttribute(kvm_ioctls::Error, bool, u32),
    /// The number of vCPUs in the GicState doesn't match the number of vCPUs on the system
    #[error(
        "The number of vCPUs in the GicState doesn't match the number of vCPUs on the system."
    )]
    InconsistentVcpuCount,
    /// The VgicSysRegsState is invalid
    #[error("The VgicSysRegsState is invalid.")]
    InvalidVgicSysRegState,
}
type Result<T> = result::Result<T, Error>;

/// List of implemented GICs.
pub enum GICVersion {
    /// Legacy version.
    GICV2,
    /// GICV3 without ITS.
    GICV3,
}

/// Trait for GIC devices.
pub trait GICDevice {
    /// Returns the file descriptor of the GIC device
    fn device_fd(&self) -> &DeviceFd;

    /// Returns an array with GIC device properties
    fn device_properties(&self) -> &[u64];

    /// Returns the number of vCPUs this GIC handles
    fn vcpu_count(&self) -> u64;

    /// Returns the fdt compatibility property of the device
    fn fdt_compatibility(&self) -> &str;

    /// Returns the maint_irq fdt property of the device
    fn fdt_maint_irq(&self) -> u32;

    /// Returns the GIC version of the device
    fn version() -> u32
    where
        Self: Sized;

    /// Create the GIC device object
    fn create_device(fd: DeviceFd, vcpu_count: u64) -> Box<dyn GICDevice>
    where
        Self: Sized;

    /// Setup the device-specific attributes
    fn init_device_attributes(gic_device: &dyn GICDevice) -> Result<()>
    where
        Self: Sized;

    /// Initialize a GIC device
    fn init_device(vm: &VmFd) -> Result<DeviceFd>
    where
        Self: Sized,
    {
        let mut gic_device = kvm_bindings::kvm_create_device {
            type_: Self::version(),
            fd: 0,
            flags: 0,
        };

        vm.create_device(&mut gic_device).map_err(Error::CreateGIC)
    }

    /// Set a GIC device attribute
    fn set_device_attribute(
        fd: &DeviceFd,
        group: u32,
        attr: u64,
        addr: u64,
        flags: u32,
    ) -> Result<()>
    where
        Self: Sized,
    {
        let attr = kvm_bindings::kvm_device_attr {
            flags,
            group,
            attr,
            addr,
        };
        fd.set_device_attr(&attr)
            .map_err(|err| Error::DeviceAttribute(err, true, group))?;

        Ok(())
    }

    /// Finalize the setup of a GIC device
    fn finalize_device(gic_device: &dyn GICDevice) -> Result<()>
    where
        Self: Sized,
    {
        // On arm there are 3 types of interrupts: SGI (0-15), PPI (16-31), SPI (32-1020).
        // SPIs are used to signal interrupts from various peripherals accessible across
        // the whole system so these are the ones that we increment when adding a new virtio device.
        // KVM_DEV_ARM_VGIC_GRP_NR_IRQS sets the highest SPI number. Consequently, we will have a
        // total of `super::layout::IRQ_MAX - 32` usable SPIs in our microVM.
        let nr_irqs: u32 = super::layout::IRQ_MAX;
        let nr_irqs_ptr = &nr_irqs as *const u32;
        Self::set_device_attribute(
            gic_device.device_fd(),
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            0,
            nr_irqs_ptr as u64,
            0,
        )?;

        // Finalize the GIC.
        // See https://code.woboq.org/linux/linux/virt/kvm/arm/vgic/vgic-kvm-device.c.html#211.
        Self::set_device_attribute(
            gic_device.device_fd(),
            kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
            0,
            0,
        )?;

        Ok(())
    }

    /// Method to save the state of the GIC device.
    fn save_device(&self, mpidrs: &[u64]) -> Result<GicState>;

    /// Method to restore the state of the GIC device.
    fn restore_device(&self, mpidrs: &[u64], state: &GicState) -> Result<()>;

    /// Method to initialize the GIC device
    fn create(vm: &VmFd, vcpu_count: u64) -> Result<Box<dyn GICDevice>>
    where
        Self: Sized,
    {
        let vgic_fd = Self::init_device(vm)?;

        let device = Self::create_device(vgic_fd, vcpu_count);

        Self::init_device_attributes(device.as_ref())?;

        Self::finalize_device(device.as_ref())?;

        Ok(device)
    }
}

/// Create a GIC device.

/// If "version" parameter is "None" the function will try to create by default a GICv3 device.
/// If that fails it will try to fall-back to a GICv2 device.
/// If version is Some the function will try to create a device of exactly the specified version.
pub fn create_gic(
    vm: &VmFd,
    vcpu_count: u64,
    version: Option<GICVersion>,
) -> Result<Box<dyn GICDevice>> {
    match version {
        Some(GICVersion::GICV2) => GICv2::create(vm, vcpu_count),
        Some(GICVersion::GICV3) => GICv3::create(vm, vcpu_count),
        None => GICv3::create(vm, vcpu_count).or_else(|_| GICv2::create(vm, vcpu_count)),
    }
}

#[cfg(test)]
mod tests {

    use kvm_ioctls::Kvm;

    use super::*;

    #[test]
    fn test_create_gic() {
        let kvm = Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        assert!(create_gic(&vm, 1, None).is_ok());
    }
}
