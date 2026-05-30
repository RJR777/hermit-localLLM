#![allow(dead_code)]

use alloc::vec::Vec;
use core::fmt;

use ahash::RandomState;
use hashbrown::HashMap;
#[cfg(any(
	feature = "virtio-fs",
	feature = "virtio-vsock",
	feature = "virtio-console"
))]
use hermit_sync::InterruptTicketMutex;
use hermit_sync::without_interrupts;
use memory_addresses::{PhysAddr, VirtAddr};
use pci_types::capability::CapabilityIterator;
use pci_types::{
	Bar, CommandRegister, ConfigRegionAccess, DeviceId, EndpointHeader, InterruptLine,
	InterruptPin, MAX_BARS, PciAddress, PciHeader, StatusRegister, VendorId,
};

use crate::arch::pci::PciConfigRegion;
#[cfg(feature = "virtio-console")]
use crate::console::IoDevice;
#[cfg(feature = "virtio-console")]
use crate::drivers::console::{VirtioConsoleDriver, VirtioUART};
#[cfg(feature = "virtio-fs")]
use crate::drivers::fs::VirtioFsDriver;
#[cfg(feature = "rtl8139")]
use crate::drivers::net::rtl8139::{self, RTL8139Driver};
#[cfg(feature = "rtl8152")]
use crate::drivers::net::rtl8152::Rtl8152NetworkDriver;
#[cfg(feature = "rtl8169")]
use crate::drivers::net::rtl8169::{self, RTL8169Driver};
#[cfg(all(
	not(feature = "rtl8169"),
	not(feature = "rtl8139"),
	not(feature = "rtl8152"),
	feature = "virtio-net"
))]
use crate::drivers::net::virtio::VirtioNetDriver;
#[cfg(feature = "virtio")]
use crate::drivers::virtio::transport::pci as pci_virtio;
#[cfg(feature = "virtio")]
#[allow(unused_imports)]
use crate::drivers::virtio::transport::pci::VirtioDriver;
#[cfg(feature = "virtio-vsock")]
use crate::drivers::vsock::VirtioVsockDriver;
#[allow(unused_imports)]
use crate::drivers::{Driver, InterruptHandlerQueue};
#[cfg(any(
	feature = "rtl8169",
	feature = "rtl8139",
	feature = "rtl8152",
	feature = "virtio-net"
))]
use crate::executor::device::NETWORK_DEVICE;
use crate::init_cell::InitCell;

pub(crate) static PCI_DEVICES: InitCell<Vec<PciDevice<PciConfigRegion>>> =
	InitCell::new(Vec::new());
static PCI_DRIVERS: InitCell<Vec<PciDriver>> = InitCell::new(Vec::new());

#[derive(Copy, Clone, Debug)]
pub(crate) struct PciDevice<T: ConfigRegionAccess> {
	address: PciAddress,
	access: T,
}

impl<T: ConfigRegionAccess> PciDevice<T> {
	pub const fn new(address: PciAddress, access: T) -> Self {
		Self { address, access }
	}

	pub fn access(&self) -> &T {
		&self.access
	}

	pub fn address(&self) -> PciAddress {
		self.address
	}

	pub fn header(&self) -> PciHeader {
		PciHeader::new(self.address)
	}

	pub fn revision_and_class(&self) -> (u8, u8, u8, u8) {
		self.header().revision_and_class(&self.access)
	}

	/// Set flag to the command register
	pub fn set_command(&self, cmd: CommandRegister) {
		self.header()
			.update_command(&self.access, |command| command | cmd);
	}

	/// Returns the bar at bar-register `slot`.
	pub fn get_bar(&self, slot: u8) -> Option<Bar> {
		let header = self.header();
		let endpoint = EndpointHeader::from_header(header, &self.access)?;
		endpoint.bar(slot, &self.access)
	}

	/// Configure the bar at register `slot`
	pub fn set_bar(&self, slot: u8, bar: Bar) {
		let value = match bar {
			Bar::Io { port } => (port | 1) as usize,
			Bar::Memory32 {
				address,
				size: _,
				prefetchable,
			} => {
				if prefetchable {
					(address | (1 << 3)) as usize
				} else {
					address as usize
				}
			}
			Bar::Memory64 {
				address,
				size: _,
				prefetchable,
			} => {
				if prefetchable {
					(address | (2 << 1) | (1 << 3)) as usize
				} else {
					(address | (2 << 1)) as usize
				}
			}
		};
		let mut header = EndpointHeader::from_header(self.header(), &self.access).unwrap();
		unsafe {
			header.write_bar(slot, &self.access, value).unwrap();
		}
	}

	/// Memory maps pci bar with specified index to identical location in virtual memory.
	/// no_cache determines if we set the `Cache Disable` flag in the page-table-entry.
	/// Returns (virtual-pointer, size) if successful, else None (if bar non-existent or IOSpace)
	pub fn memory_map_bar(&self, index: u8, no_cache: bool) -> Option<(VirtAddr, usize)> {
		let (address, size, prefetchable, _width) = match self.get_bar(index) {
			Some(Bar::Io { .. }) => {
				warn!("Cannot map IOBar!");
				return None;
			}
			Some(Bar::Memory32 {
				address,
				size,
				prefetchable,
			}) => (
				u64::from(address),
				usize::try_from(size).unwrap(),
				prefetchable,
				32,
			),
			Some(Bar::Memory64 {
				address,
				size,
				prefetchable,
			}) => (address, usize::try_from(size).unwrap(), prefetchable, 64),
			_ => {
				return None;
			}
		};

		if address == 0 {
			return None;
		}

		debug!("Mapping bar {index} at {address:#x} with length {size:#x}");

		if !prefetchable {
			warn!("Currently only mapping of prefetchable bars is supported!");
		}

		// Since the bios/bootloader manages the physical address space, the address got from the bar is unique and not overlapping.
		// We therefore do not need to reserve any additional memory in our kernel.
		// Map bar into RW^X virtual memory
		let physical_address = address;
		let virtual_address =
			crate::mm::map(PhysAddr::new(physical_address), size, true, true, no_cache);

		Some((virtual_address, size))
	}

	pub fn get_irq(&self) -> Option<InterruptLine> {
		let header = self.header();
		let endpoint = EndpointHeader::from_header(header, &self.access)?;
		let (pin, line) = endpoint.interrupt(&self.access);
		// PCIe specification v5 section 7.5.1.1.13 (Interrupt Pin Register)
		match pin {
			0 => {
				warn!("The function uses no legacy interrupt message(s).");
				None
			}
			1..=4 => {
				// PCI specification v3 footnote 43
				#[cfg(target_arch = "x86_64")]
				if matches!(line, 16..254) {
					error!("Reserved IRQ number");
					return None;
				} else if line == 255 {
					error!("Unknown IRQ line or no connection to the interrupt controller");
					return None;
				}

				Some(line)
			}
			5.. => {
				error!("Reserved interrupt pin value returned.");
				None
			}
		}
	}

	pub fn set_irq(&self, pin: InterruptPin, line: InterruptLine) {
		let mut header = EndpointHeader::from_header(self.header(), &self.access).unwrap();
		header.update_interrupt(&self.access, |(_pin, _line)| (pin, line));
	}

	pub fn device_id(&self) -> DeviceId {
		let (_vendor_id, device_id) = self.header().id(&self.access);
		device_id
	}

	pub fn id(&self) -> (VendorId, DeviceId) {
		self.header().id(&self.access)
	}

	pub fn status(&self) -> StatusRegister {
		self.header().status(&self.access)
	}

	pub fn capabilities(&self) -> Option<CapabilityIterator<&T>> {
		EndpointHeader::from_header(self.header(), &self.access)
			.map(|header| header.capabilities(&self.access))
	}
}

impl<T: ConfigRegionAccess> fmt::Display for PciDevice<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let header = self.header();
		let header_type = header.header_type(&self.access);
		let (vendor_id, device_id) = header.id(&self.access);
		let (_dev_rev, class_id, subclass_id, _interface) = header.revision_and_class(&self.access);

		if let Some(endpoint) = EndpointHeader::from_header(header, &self.access) {
			#[cfg(feature = "pci-ids")]
			let (class_name, vendor_name, device_name) = {
				use pci_ids::{Class, Device, FromId, Subclass};

				let class_name = Class::from_id(class_id).map_or("Unknown Class", |class| {
					class
						.subclasses()
						.find(|s| s.id() == subclass_id)
						.map(Subclass::name)
						.unwrap_or_else(|| class.name())
				});

				let (vendor_name, device_name) = Device::from_vid_pid(vendor_id, device_id)
					.map(|device| (device.vendor().name(), device.name()))
					.unwrap_or(("Unknown Vendor", "Unknown Device"));

				(class_name, vendor_name, device_name)
			};

			#[cfg(not(feature = "pci-ids"))]
			let (class_name, vendor_name, device_name) =
				("Unknown Class", "Unknown Vendor", "Unknown Device");

			// Output detailed readable information about this device.
			write!(
				f,
				"{:02X}:{:02X} {} [{:02X}{:02X}]: {} {} [{:04X}:{:04X}]",
				self.address.bus(),
				self.address.device(),
				class_name,
				class_id,
				subclass_id,
				vendor_name,
				device_name,
				vendor_id,
				device_id
			)?;

			// If the devices uses an IRQ, output this one as well.
			let (_, irq) = endpoint.interrupt(&self.access);
			if irq != 0 && irq != u8::MAX {
				write!(f, ", IRQ {irq}")?;
			}

			let mut slot: u8 = 0;
			while usize::from(slot) < MAX_BARS {
				if let Some(pci_bar) = endpoint.bar(slot, &self.access) {
					match pci_bar {
						Bar::Memory64 {
							address,
							size,
							prefetchable,
						} => {
							write!(
								f,
								", BAR{slot} Memory64 {{ address: {address:#X}, size: {size:#X}, prefetchable: {prefetchable} }}"
							)?;
							slot += 1;
						}
						Bar::Memory32 {
							address,
							size,
							prefetchable,
						} => {
							write!(
								f,
								", BAR{slot} Memory32 {{ address: {address:#X}, size: {size:#X}, prefetchable: {prefetchable} }}"
							)?;
						}
						Bar::Io { port } => {
							write!(f, ", BAR{slot} IO {{ port: {port:#X} }}")?;
						}
					}
				}
				slot += 1;
			}
		} else {
			// Output detailed readable information about this device.
			write!(
				f,
				"{:02X}:{:02X} {:?} [{:04X}:{:04X}]",
				self.address.bus(),
				self.address.device(),
				header_type,
				vendor_id,
				device_id
			)?;
		}

		Ok(())
	}
}

pub(crate) fn print_information() {
	infoheader!(" PCI BUS INFORMATION ");

	for adapter in PCI_DEVICES.finalize().iter() {
		print!(".");
		info!("{adapter}");
	}

	print_boot_device_discovery();

	infofooter!();
}

fn print_boot_device_discovery() {
	const SERIAL_BUS_CONTROLLER: u8 = 0x0c;
	const USB_CONTROLLER: u8 = 0x03;
	const SMBUS_CONTROLLER: u8 = 0x05;
	const OTHER_SERIAL_BUS_CONTROLLER: u8 = 0x80;
	const XHCI_INTERFACE: u8 = 0x30;

	let mut found_xhci = false;
	let mut found_possible_i2c = false;

	for adapter in PCI_DEVICES.finalize().iter() {
		let (_revision, class_id, subclass_id, interface) = adapter.revision_and_class();
		match (class_id, subclass_id, interface) {
			(SERIAL_BUS_CONTROLLER, USB_CONTROLLER, XHCI_INTERFACE) => {
				found_xhci = true;
				info!(
					"Discovered xHCI USB controller at {} ({adapter})",
					adapter.address()
				);
			}
			(SERIAL_BUS_CONTROLLER, SMBUS_CONTROLLER | OTHER_SERIAL_BUS_CONTROLLER, _) => {
				found_possible_i2c = true;
				info!(
					"Discovered possible I2C/Serial IO controller at {} ({adapter})",
					adapter.address()
				);
			}
			_ => {}
		}
	}

	if found_xhci {
		info!(
			"RTL8152/RTL8153 discovery requires USB descriptor enumeration after xHCI port discovery."
		);
	} else {
		warn!("No xHCI USB controller discovered.");
	}

	if !found_possible_i2c {
		info!("No PCI SMBus/other Serial IO controller discovered for I2C-HID keyboard clues.");
	}
}

#[allow(clippy::large_enum_variant)]
#[allow(clippy::enum_variant_names)]
#[non_exhaustive]
pub(crate) enum PciDriver {
	#[cfg(feature = "virtio-fs")]
	VirtioFs(InterruptTicketMutex<VirtioFsDriver>),
	#[cfg(feature = "virtio-console")]
	VirtioConsole(InterruptTicketMutex<VirtioConsoleDriver>),
	#[cfg(feature = "virtio-vsock")]
	VirtioVsock(InterruptTicketMutex<VirtioVsockDriver>),
	Xhci(u8), // IRQ number
}

impl PciDriver {
	#[cfg(feature = "virtio-console")]
	fn get_console_driver(&self) -> Option<&InterruptTicketMutex<VirtioConsoleDriver>> {
		#[allow(unreachable_patterns)]
		match self {
			Self::VirtioConsole(drv) => Some(drv),
			_ => None,
		}
	}

	#[cfg(feature = "virtio-vsock")]
	fn get_vsock_driver(&self) -> Option<&InterruptTicketMutex<VirtioVsockDriver>> {
		#[allow(unreachable_patterns)]
		match self {
			Self::VirtioVsock(drv) => Some(drv),
			_ => None,
		}
	}

	#[cfg(feature = "virtio-fs")]
	fn get_filesystem_driver(&self) -> Option<&InterruptTicketMutex<VirtioFsDriver>> {
		match self {
			Self::VirtioFs(drv) => Some(drv),
			#[allow(unreachable_patterns)]
			_ => None,
		}
	}

	fn get_interrupt_handler(&self) -> (InterruptLine, fn()) {
		#[allow(unreachable_patterns)]
		match self {
			#[cfg(feature = "virtio-vsock")]
			Self::VirtioVsock(drv) => {
				fn vsock_handler() {
					let Some(driver) = get_vsock_driver() else {
						return;
					};

					driver.lock().handle_interrupt();
				}

				let irq_number = drv.lock().get_interrupt_number();

				(irq_number, vsock_handler)
			}
			#[cfg(feature = "virtio-fs")]
			Self::VirtioFs(drv) => {
				fn virtio_fs_handler() {
					let Some(driver) = get_filesystem_driver() else {
						return;
					};

					driver.lock().handle_interrupt();
				}

				let irq_number = drv.lock().get_interrupt_number();

				(irq_number, virtio_fs_handler)
			}
			#[cfg(feature = "virtio-console")]
			Self::VirtioConsole(drv) => {
				fn console_handler() {
					let Some(driver) = get_console_driver() else {
						return;
					};

					driver.lock().handle_interrupt();
				}

				let irq_number = drv.lock().get_interrupt_number();

				(irq_number, console_handler)
			}
			Self::Xhci(irq) => {
				let irq = *irq;
				(irq, crate::drivers::usb::xhci::handle_interrupt)
			}
			_ => todo!(),
		}
	}
}

pub(crate) fn register_driver(drv: PciDriver) {
	PCI_DRIVERS.with(|pci_drivers| pci_drivers.unwrap().push(drv));
}

pub(crate) fn get_interrupt_handlers() -> HashMap<InterruptLine, InterruptHandlerQueue, RandomState>
{
	let mut handlers: HashMap<InterruptLine, InterruptHandlerQueue, RandomState> =
		HashMap::with_hasher(RandomState::with_seeds(0, 0, 0, 0));

	for drv in PCI_DRIVERS.finalize().iter() {
		let (irq_number, handler) = drv.get_interrupt_handler();

		handlers.entry(irq_number).or_default().push_back(handler);
	}

	#[cfg(target_arch = "x86_64")]
	{
		use crate::kernel::serial::get_serial_handler;
		if let Some((irq_number, handler)) = get_serial_handler() {
			handlers.entry(irq_number).or_default().push_back(handler);
		}
	}

	#[cfg(any(
		feature = "rtl8169",
		feature = "rtl8139",
		feature = "rtl8152",
		feature = "virtio-net"
	))]
	if let Some(device) = NETWORK_DEVICE.lock().as_ref() {
		handlers
			.entry(device.get_interrupt_number())
			.or_default()
			.push_back(crate::executor::network::network_handler);
	}

	handlers
}

#[cfg(all(
	not(feature = "rtl8169"),
	not(feature = "rtl8139"),
	not(feature = "rtl8152"),
	feature = "virtio-net"
))]
pub(crate) type NetworkDevice = VirtioNetDriver;

#[cfg(feature = "rtl8169")]
pub(crate) type NetworkDevice = RTL8169Driver;

#[cfg(feature = "rtl8139")]
pub(crate) type NetworkDevice = RTL8139Driver;

#[cfg(feature = "rtl8152")]
pub(crate) type NetworkDevice = Rtl8152NetworkDriver<crate::drivers::usb::xhci::HermitDma>;

#[cfg(feature = "virtio-console")]
pub(crate) fn get_console_driver() -> Option<&'static InterruptTicketMutex<VirtioConsoleDriver>> {
	PCI_DRIVERS
		.get()?
		.iter()
		.find_map(|drv| drv.get_console_driver())
}

#[cfg(feature = "virtio-vsock")]
pub(crate) fn get_vsock_driver() -> Option<&'static InterruptTicketMutex<VirtioVsockDriver>> {
	PCI_DRIVERS
		.get()?
		.iter()
		.find_map(|drv| drv.get_vsock_driver())
}

#[cfg(feature = "virtio-fs")]
pub(crate) fn get_filesystem_driver() -> Option<&'static InterruptTicketMutex<VirtioFsDriver>> {
	PCI_DRIVERS
		.get()?
		.iter()
		.find_map(|drv| drv.get_filesystem_driver())
}

pub(crate) fn init() {
	without_interrupts(|| {
		// Searching for Realtek RTL8169/RTL8111/RTL8168
		#[cfg(feature = "rtl8169")]
		for adapter in PCI_DEVICES.finalize().iter().filter(|x| {
			let (vendor_id, device_id) = x.id();
			vendor_id == 0x10ec && matches!(device_id, 0x8161 | 0x8167 | 0x8168 | 0x8169)
		}) {
			info!(
				"Found Realtek RTL8169-family network device with device id {:#x}",
				adapter.device_id()
			);

			match rtl8169::init_device(adapter) {
				Ok(drv) => *crate::executor::device::NETWORK_DEVICE.lock() = Some(drv),
				Err(err) => error!("Could not initialize rtl8169 device: {err}"),
			}
		}

		// Searching for Realtek RTL8139
		#[cfg(feature = "rtl8139")]
		for adapter in PCI_DEVICES.finalize().iter().filter(|x| {
			print!(".");
			let (vendor_id, device_id) = x.id();
			vendor_id == 0x10ec && (0x8138..=0x8139).contains(&device_id)
		}) {
			info!(
				"Found Realtek network device with device id {:#x}",
				adapter.device_id()
			);

			match rtl8139::init_device(adapter) {
				Ok(drv) => *crate::executor::device::NETWORK_DEVICE.lock() = Some(drv),
				Err(err) => error!("Could not initialize rtl8139 device: {err}"),
			}
		}

		// Searching for AHCI before xHCI keeps SSD cache probing deterministic.
		for adapter in PCI_DEVICES.finalize().iter().filter(|x| {
			let header = x.header();
			let (_rev, class_id, subclass_id, interface) = header.revision_and_class(x.access());
			if class_id == 0x01 {
				info!(
					"Found Storage controller at {} with device id {:#x} (class={:#x}, subclass={:#x}, interface={:#x})",
					x.address(),
					x.device_id(),
					class_id,
					subclass_id,
					interface
				);
			}
			class_id == 0x01 && subclass_id == 0x06 && interface == 0x01
		}) {
			info!(
				"Found AHCI SATA controller at {} with device id {:#x}",
				adapter.address(),
				adapter.device_id()
			);

			crate::drivers::storage::ahci::enumerate_controller(adapter);
		}

		// Searching for USB xHCI
		for adapter in PCI_DEVICES.finalize().iter().filter(|x| {
			let header = x.header();
			let (_rev, class_id, subclass_id, interface) = header.revision_and_class(x.access());
			class_id == 0x0c && subclass_id == 0x03 && interface == 0x30
		}) {
			info!(
				"Found USB xHCI controller with device id {:#x}",
				adapter.device_id()
			);

			// Intel Sunrise Point Port-Switching (Hot-Wire)
			let header = adapter.header();
			let (vendor_id, device_id) = header.id(adapter.access());
			if vendor_id == 0x8086 && (device_id == 0x9d2f || device_id == 0xa12f) {
				info!("PCI: Intel Sunrise Point xHCI found. Performing Port-Switching...");
				unsafe {
					// 1. Route all USB 2.0 ports to xHCI
					adapter.access().write(adapter.address, 0xd0, 0xffffffff);
					// 2. Set USB 2.0 Port Routing Mask
					adapter.access().write(adapter.address, 0xd8, 0xffffffff);
					// 3. Set USB 3.0 Port Routing Mask (SuperSpeed)
					adapter.access().write(adapter.address, 0x38, 0xffffffff);
					info!("PCI: Port-Switching EXECUTED. Pins connected to xHCI.");

					// Force PCI Power Management to D0 (Active)
					// Offset 0x70 is typical for Sunrise Point PMCAP
					adapter.access().write(adapter.address, 0x74, 0x0);
					info!("PCI: Forcing D0 Power State... Chip Woken UP.");
				}
			}

			// Intel xHCI Port-Switching (Hot-Wire)
			// Target: Route USB 2.0 and 3.0 ports to xHCI instead of EHCI
			let (vendor_id, device_id) = adapter.id();
			if vendor_id == 0x8086 {
				info!(
					"PCI: Intel xHCI detected ({:#x}). Checking port routing...",
					device_id
				);
				unsafe {
					// 1. Route all USB 2.0 ports to xHCI (XUSB2PR)
					adapter.access().write(adapter.address, 0xd0, 0xffffffff);
					// 2. Set USB 2.0 Port Routing Mask (XUSB2PRM)
					adapter.access().write(adapter.address, 0xd8, 0xffffffff);
					// 3. Set USB 3.0 Port Routing Mask (SuperSpeed - PSSEN)
					adapter.access().write(adapter.address, 0x38, 0xffffffff);
					info!(
						"PCI: Port-Switching EXECUTED. USB 2.0 (0-12) and 3.0 ports connected to xHCI."
					);

					// Force PCI Power Management to D0 (Active)
					adapter.access().write(adapter.address, 0x74, 0x0);
				}
			}

			// If no legacy IRQ, try to force MSI
			if adapter.get_irq().is_none() {
				info!("PCI: No legacy IRQ for xHCI. Attempting to enable MSI...");
				// Basic MSI enablement (Vector 0x24)
				// Offset 0x80 is common for MSI cap on Intel xHCI
				unsafe {
					adapter.access().write(adapter.address, 0x84, 0xfee00000); // Address (Local APIC)
					adapter.access().write(adapter.address, 0x88, 0xe0); // Data (Vector 0xe0)
					let ctrl = adapter.access().read(adapter.address, 0x80);
					adapter
						.access()
						.write(adapter.address, 0x80, ctrl | 0x10000); // Enable MSI
				}
				crate::drivers::usb::xhci::init_device_with_irq(adapter, 0xe0 - 32);
			} else {
				let irq = adapter.get_irq().unwrap();
				crate::drivers::usb::xhci::init_device_with_irq(adapter, irq);
			}
		}
	});
}

/// A module containing PCI specific errors
///
/// Errors include...
pub(crate) mod error {
	use thiserror::Error;

	/// An enum of PciErrors
	/// typically carrying the device's id as an u16.
	#[derive(Error, Debug)]
	pub enum PciError {
		#[error("Driver failed to initialize device with id: {0:#x}. Due to unknown reasosn!")]
		General(u16),
		#[error("Driver failed to initialize device with id: {0:#x}. Reason: No BAR's found.")]
		NoBar(u16),
		#[error(
			"Driver failed to initialize device with id: {0:#x}. Reason: No Capabilities pointer found."
		)]
		NoCapPtr(u16),
		#[error(
			"Driver failed to initialize device with id: {0:#x}. Reason: No Virtio capabilities were found."
		)]
		NoVirtioCaps(u16),
	}
}
