#![allow(dead_code)]

use core::hint::spin_loop;
use core::ptr::NonNull;

use pci_types::{Bar, CommandRegister, InterruptLine, MAX_BARS};
use smoltcp::phy::DeviceCapabilities;
use thiserror::Error;
use volatile::access::{NoAccess, ReadOnly, ReadWrite};
use volatile::{VolatileFieldAccess, VolatileRef};

use crate::arch::pci::PciConfigRegion;
use crate::drivers::Driver;
use crate::drivers::error::DriverError;
use crate::drivers::net::{NetworkDriver, mtu};
use crate::drivers::pci::PciDevice;

const CR_RST: u8 = 0x10;
const CR_RE: u8 = 0x08;
const CR_TE: u8 = 0x04;
const RTL8169_RESET_SPINS: usize = 100_000;

#[repr(C)]
#[derive(VolatileFieldAccess)]
struct Regs {
	#[access(ReadOnly)]
	mac0: u8,
	#[access(ReadOnly)]
	mac1: u8,
	#[access(ReadOnly)]
	mac2: u8,
	#[access(ReadOnly)]
	mac3: u8,
	#[access(ReadOnly)]
	mac4: u8,
	#[access(ReadOnly)]
	mac5: u8,
	#[access(NoAccess)]
	__reserved0: [u8; 0x31],
	#[access(ReadWrite)]
	cr: u8,
	#[access(NoAccess)]
	__reserved1: [u8; 0x08],
	#[access(ReadWrite)]
	imr: u16,
	#[access(ReadWrite)]
	isr: u16,
}

#[derive(Error, Debug)]
pub enum RTL8169Error {
	#[error("initialization failed")]
	InitFailed,
	#[error("reset failed")]
	ResetFailed,
	#[error("unknown RTL8169 error")]
	Unknown,
}

pub(crate) struct RTL8169Driver {
	regs: VolatileRef<'static, Regs>,
	mtu: u16,
	irq: InterruptLine,
	mac: [u8; 6],
}

pub struct RTL8169RxToken;

impl smoltcp::phy::RxToken for RTL8169RxToken {
	fn consume<R, F>(self, f: F) -> R
	where
		F: FnOnce(&[u8]) -> R,
	{
		f(&[])
	}
}

pub struct RTL8169TxToken;

impl smoltcp::phy::TxToken for RTL8169TxToken {
	fn consume<R, F>(self, len: usize, f: F) -> R
	where
		F: FnOnce(&mut [u8]) -> R,
	{
		let mut buf = alloc::vec![0u8; len];
		f(&mut buf)
	}
}

impl smoltcp::phy::Device for RTL8169Driver {
	type RxToken<'a>
		= RTL8169RxToken
	where
		Self: 'a;
	type TxToken<'a>
		= RTL8169TxToken
	where
		Self: 'a;

	fn receive(
		&mut self,
		_: smoltcp::time::Instant,
	) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
		None
	}

	fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
		None
	}

	fn capabilities(&self) -> DeviceCapabilities {
		let mut caps = DeviceCapabilities::default();
		caps.medium = smoltcp::phy::Medium::Ethernet;
		caps.max_transmission_unit = usize::from(self.mtu);
		caps
	}
}

impl NetworkDriver for RTL8169Driver {
	fn get_mac_address(&self) -> [u8; 6] {
		self.mac
	}

	fn has_packet(&self) -> bool {
		false
	}

	fn set_polling_mode(&mut self, _value: bool) {}

	fn handle_interrupt(&mut self) {}
}

impl Driver for RTL8169Driver {
	fn get_interrupt_number(&self) -> InterruptLine {
		self.irq
	}

	fn get_name(&self) -> &'static str {
		"rtl8169"
	}
}

pub(crate) fn init_device(
	device: &PciDevice<PciConfigRegion>,
) -> Result<RTL8169Driver, DriverError> {
	let irq = device
		.get_irq()
		.ok_or(DriverError::InitRTL8169DevFail(RTL8169Error::Unknown))?;
	let mut regs = None;

	for i in 0..MAX_BARS {
		match device.get_bar(i.try_into().unwrap()) {
			Some(Bar::Memory32 { .. }) | Some(Bar::Memory64 { .. }) => {
				let (addr, _size) = device.memory_map_bar(i.try_into().unwrap(), true).unwrap();
				regs = Some(unsafe { VolatileRef::new(NonNull::new(addr.as_mut_ptr()).unwrap()) });
				break;
			}
			_ => {}
		}
	}

	let mut regs = regs.ok_or(DriverError::InitRTL8169DevFail(RTL8169Error::Unknown))?;

	device.set_command(CommandRegister::BUS_MASTER_ENABLE | CommandRegister::MEMORY_ENABLE);

	let mac = [
		regs.as_ptr().mac0().read(),
		regs.as_ptr().mac1().read(),
		regs.as_ptr().mac2().read(),
		regs.as_ptr().mac3().read(),
		regs.as_ptr().mac4().read(),
		regs.as_ptr().mac5().read(),
	];

	if mac == [0; 6] {
		return Err(DriverError::InitRTL8169DevFail(RTL8169Error::InitFailed));
	}

	regs.as_mut_ptr().cr().write(CR_RST);
	let mut spins = RTL8169_RESET_SPINS;
	while (regs.as_ptr().cr().read() & CR_RST) != 0 && spins > 0 {
		spin_loop();
		spins -= 1;
	}
	if spins == 0 {
		return Err(DriverError::InitRTL8169DevFail(RTL8169Error::ResetFailed));
	}

	regs.as_mut_ptr().imr().write(0);
	regs.as_mut_ptr().isr().write(u16::MAX);
	regs.as_mut_ptr().cr().write(CR_RE | CR_TE);

	info!(
		"RTL8169: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} CR={:#04x} IRQ={}",
		mac[0],
		mac[1],
		mac[2],
		mac[3],
		mac[4],
		mac[5],
		regs.as_ptr().cr().read(),
		irq,
	);

	Ok(RTL8169Driver {
		regs,
		mtu: mtu(),
		irq,
		mac,
	})
}
