use alloc::collections::VecDeque;
use core::mem::size_of;
use core::ptr;

use embedded_io::{ErrorType, Read, ReadReady, Write};
use hermit_sync::{InterruptTicketMutex, Lazy};

#[cfg(feature = "pci")]
use crate::arch::x86_64::kernel::interrupts;
#[cfg(feature = "pci")]
use crate::drivers::InterruptLine;
use crate::errno::Errno;

#[cfg(feature = "pci")]
const SERIAL_IRQ: u8 = 4;
const VIRTUAL_CONSOLE_MAGIC: u32 = 0x5643_4f4e;
const VIRTUAL_CONSOLE_VERSION: u32 = 1;
const VIRTUAL_CONSOLE_PAGE_SIZE: usize = 4096;
const VIRTUAL_CONSOLE_RING_SIZE: usize = (VIRTUAL_CONSOLE_PAGE_SIZE - 8 * size_of::<u32>()) / 2;

static UART_DEVICE: Lazy<InterruptTicketMutex<UartDevice>> =
	Lazy::new(|| unsafe { InterruptTicketMutex::new(UartDevice::new()) });

struct UartDevice {
	pub uart: uart_16550::SerialPort,
	pub buffer: VecDeque<u8>,
}

impl UartDevice {
	pub unsafe fn new() -> Self {
		let base = crate::env::boot_info()
			.hardware_info
			.serial_port_base
			.expect("serial backend selected without a serial port base")
			.get();
		let mut uart = unsafe { uart_16550::SerialPort::new(base) };
		uart.init();

		Self {
			uart,
			buffer: VecDeque::new(),
		}
	}
}

#[repr(C)]
struct VirtualConsolePage {
	magic: u32,
	version: u32,
	tx_write: u32,
	tx_read: u32,
	rx_write: u32,
	rx_read: u32,
	flags: u32,
	reserved: u32,
	tx_buf: [u8; VIRTUAL_CONSOLE_RING_SIZE],
	rx_buf: [u8; VIRTUAL_CONSOLE_RING_SIZE],
}

#[derive(Clone, Copy)]
struct MemoryConsole {
	page: *mut VirtualConsolePage,
}

impl MemoryConsole {
	fn new(address: usize) -> Option<Self> {
		let page = ptr::with_exposed_provenance_mut::<VirtualConsolePage>(address);
		let magic = unsafe { ptr::addr_of!((*page).magic).read_volatile() };
		let version = unsafe { ptr::addr_of!((*page).version).read_volatile() };
		if magic != VIRTUAL_CONSOLE_MAGIC || version != VIRTUAL_CONSOLE_VERSION {
			return None;
		}

		Some(Self { page })
	}

	fn rx_len(&self) -> usize {
		let write = unsafe { ptr::addr_of!((*self.page).rx_write).read_volatile() };
		let read = unsafe { ptr::addr_of!((*self.page).rx_read).read_volatile() };
		let used = write.wrapping_sub(read);
		used.min(VIRTUAL_CONSOLE_RING_SIZE as u32) as usize
	}

	fn read(&self, buf: &mut [u8]) -> usize {
		let mut read = unsafe { ptr::addr_of!((*self.page).rx_read).read_volatile() };
		let write = unsafe { ptr::addr_of!((*self.page).rx_write).read_volatile() };
		let available = write
			.wrapping_sub(read)
			.min(VIRTUAL_CONSOLE_RING_SIZE as u32) as usize;
		let count = available.min(buf.len());

		for slot in buf.iter_mut().take(count) {
			let index = (read as usize) % VIRTUAL_CONSOLE_RING_SIZE;
			*slot = unsafe { ptr::addr_of!((*self.page).rx_buf[index]).read_volatile() };
			read = read.wrapping_add(1);
		}

		unsafe {
			ptr::addr_of_mut!((*self.page).rx_read).write_volatile(read);
		}

		count
	}

	fn write(&self, buf: &[u8]) -> usize {
		for &byte in buf {
			let write = unsafe { ptr::addr_of!((*self.page).tx_write).read_volatile() };
			let mut read = unsafe { ptr::addr_of!((*self.page).tx_read).read_volatile() };
			if write.wrapping_sub(read) >= VIRTUAL_CONSOLE_RING_SIZE as u32 {
				read = read.wrapping_add(1);
				unsafe {
					ptr::addr_of_mut!((*self.page).tx_read).write_volatile(read);
				}
			}

			let index = (write as usize) % VIRTUAL_CONSOLE_RING_SIZE;
			unsafe {
				ptr::addr_of_mut!((*self.page).tx_buf[index]).write_volatile(byte);
				ptr::addr_of_mut!((*self.page).tx_write).write_volatile(write.wrapping_add(1));
			}
		}

		buf.len()
	}
}

enum Backend {
	Memory(MemoryConsole),
	Uart,
	Null,
}

fn backend() -> Backend {
	if let Some(info) = crate::env::virtual_console_info() {
		if let Ok(address) = usize::try_from(info.address.get()) {
			if let Some(console) = MemoryConsole::new(address) {
				return Backend::Memory(console);
			}
		}
	}

	if crate::env::boot_info()
		.hardware_info
		.serial_port_base
		.is_some()
	{
		Backend::Uart
	} else {
		Backend::Null
	}
}

pub(crate) struct SerialDevice;

impl SerialDevice {
	pub fn new() -> Self {
		Self {}
	}
}

impl ErrorType for SerialDevice {
	type Error = Errno;
}

impl Read for SerialDevice {
	fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
		let len = match backend() {
			Backend::Memory(console) => console.read(buf),
			Backend::Uart => UART_DEVICE.lock().buffer.read(buf)?,
			Backend::Null => 0,
		};

		Ok(len)
	}
}

impl ReadReady for SerialDevice {
	fn read_ready(&mut self) -> Result<bool, Self::Error> {
		let ready = match backend() {
			Backend::Memory(console) => console.rx_len() > 0,
			Backend::Uart => !UART_DEVICE.lock().buffer.is_empty(),
			Backend::Null => false,
		};

		Ok(ready)
	}
}

impl Write for SerialDevice {
	fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
		match backend() {
			Backend::Memory(console) => Ok(console.write(buf)),
			Backend::Uart => {
				let mut guard = UART_DEVICE.lock();
				for &data in buf {
					guard.uart.send(data);
				}
				Ok(buf.len())
			}
			Backend::Null => Ok(buf.len()),
		}
	}

	fn flush(&mut self) -> Result<(), Self::Error> {
		Ok(())
	}
}

#[cfg(feature = "pci")]
pub(crate) fn get_serial_handler() -> Option<(InterruptLine, fn())> {
	if !matches!(backend(), Backend::Uart) {
		return None;
	}

	fn serial_handler() {
		let mut guard = UART_DEVICE.lock();
		if let Ok(c) = guard.uart.try_receive() {
			guard.buffer.push_back(c);
		}

		drop(guard);
		crate::console::CONSOLE_WAKER.lock().wake();
	}

	interrupts::add_irq_name(SERIAL_IRQ, "COM1");

	Some((SERIAL_IRQ, serial_handler))
}
