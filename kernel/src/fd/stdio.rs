use alloc::boxed::Box;
use core::future;
use core::task::Poll;

use async_trait::async_trait;
use embedded_io::{Read, ReadReady, Write};
use uhyve_interface::parameters::WriteParams;
use uhyve_interface::{GuestVirtAddr, Hypercall};

use crate::console::{CONSOLE, CONSOLE_WAKER};
use crate::fd::{
	AccessPermission, FileAttr, ObjectInterface, PollEvent, STDERR_FILENO, STDOUT_FILENO,
};
use crate::io;
use crate::syscalls::interfaces::uhyve_hypercall;

pub struct GenericStdin;

#[async_trait]
impl ObjectInterface for GenericStdin {
	async fn poll(&self, event: PollEvent) -> io::Result<PollEvent> {
		let available = if physical_keyboard_read_ready() || CONSOLE.lock().read_ready()? {
			PollEvent::POLLIN | PollEvent::POLLRDNORM | PollEvent::POLLRDBAND
		} else {
			PollEvent::empty()
		};

		Ok(event & available)
	}

	async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
		future::poll_fn(|cx| {
			CONSOLE_WAKER.lock().register(cx.waker());
			let read_bytes = physical_keyboard_read(buf);
			if read_bytes > 0 {
				return Poll::Ready(Ok(read_bytes));
			}

			let read_bytes = CONSOLE.lock().read(buf)?;
			if read_bytes > 0 {
				Poll::Ready(Ok(read_bytes))
			} else if physical_keyboard_polling_enabled() {
				Poll::Ready(Ok(0))
			} else {
				Poll::Pending
			}
		})
		.await
	}

	async fn isatty(&self) -> io::Result<bool> {
		Ok(true)
	}

	async fn fstat(&self) -> io::Result<FileAttr> {
		let attr = FileAttr {
			st_mode: AccessPermission::S_IFCHR,
			..Default::default()
		};
		Ok(attr)
	}
}

impl GenericStdin {
	pub const fn new() -> Self {
		Self {}
	}
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn physical_keyboard_read_ready() -> bool {
	crate::arch::x86_64::kernel::ps2_keyboard::read_ready()
}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn physical_keyboard_read_ready() -> bool {
	false
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn physical_keyboard_polling_enabled() -> bool {
	crate::arch::x86_64::kernel::ps2_keyboard::polling_enabled()
}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn physical_keyboard_polling_enabled() -> bool {
	false
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn physical_keyboard_read(buf: &mut [u8]) -> usize {
	crate::arch::x86_64::kernel::ps2_keyboard::read(buf)
}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn physical_keyboard_read(_buf: &mut [u8]) -> usize {
	0
}

pub struct GenericStdout;

#[async_trait]
impl ObjectInterface for GenericStdout {
	async fn poll(&self, event: PollEvent) -> io::Result<PollEvent> {
		let available = PollEvent::POLLOUT | PollEvent::POLLWRNORM | PollEvent::POLLWRBAND;
		Ok(event & available)
	}

	async fn write(&self, buf: &[u8]) -> io::Result<usize> {
		let mut console = CONSOLE.lock();
		let written = console.write(buf)?;
		console.flush()?;
		Ok(written)
	}

	async fn isatty(&self) -> io::Result<bool> {
		Ok(true)
	}

	async fn fstat(&self) -> io::Result<FileAttr> {
		let attr = FileAttr {
			st_mode: AccessPermission::S_IFCHR,
			..Default::default()
		};
		Ok(attr)
	}
}

impl GenericStdout {
	pub const fn new() -> Self {
		Self {}
	}
}

pub struct GenericStderr;

#[async_trait]
impl ObjectInterface for GenericStderr {
	async fn poll(&self, event: PollEvent) -> io::Result<PollEvent> {
		let available = PollEvent::POLLOUT | PollEvent::POLLWRNORM | PollEvent::POLLWRBAND;
		Ok(event & available)
	}

	async fn write(&self, buf: &[u8]) -> io::Result<usize> {
		let mut console = CONSOLE.lock();
		let written = console.write(buf)?;
		console.flush()?;
		Ok(written)
	}

	async fn isatty(&self) -> io::Result<bool> {
		Ok(true)
	}

	async fn fstat(&self) -> io::Result<FileAttr> {
		let attr = FileAttr {
			st_mode: AccessPermission::S_IFCHR,
			..Default::default()
		};
		Ok(attr)
	}
}

impl GenericStderr {
	pub const fn new() -> Self {
		Self {}
	}
}

pub struct UhyveStdin;

#[async_trait]
impl ObjectInterface for UhyveStdin {
	async fn isatty(&self) -> io::Result<bool> {
		Ok(true)
	}

	async fn fstat(&self) -> io::Result<FileAttr> {
		let attr = FileAttr {
			st_mode: AccessPermission::S_IFCHR,
			..Default::default()
		};
		Ok(attr)
	}
}

impl UhyveStdin {
	pub const fn new() -> Self {
		Self {}
	}
}

pub struct UhyveStdout;

#[async_trait]
impl ObjectInterface for UhyveStdout {
	async fn poll(&self, event: PollEvent) -> io::Result<PollEvent> {
		let available = PollEvent::POLLOUT | PollEvent::POLLWRNORM | PollEvent::POLLWRBAND;
		Ok(event & available)
	}

	async fn write(&self, buf: &[u8]) -> io::Result<usize> {
		let write_params = WriteParams {
			fd: STDOUT_FILENO,
			buf: GuestVirtAddr::from_ptr(buf.as_ptr()),
			len: buf.len(),
		};
		uhyve_hypercall(Hypercall::FileWrite(&write_params));

		Ok(write_params.len)
	}

	async fn isatty(&self) -> io::Result<bool> {
		Ok(true)
	}

	async fn fstat(&self) -> io::Result<FileAttr> {
		let attr = FileAttr {
			st_mode: AccessPermission::S_IFCHR,
			..Default::default()
		};
		Ok(attr)
	}
}

impl UhyveStdout {
	pub const fn new() -> Self {
		Self {}
	}
}

pub struct UhyveStderr;

#[async_trait]
impl ObjectInterface for UhyveStderr {
	async fn poll(&self, event: PollEvent) -> io::Result<PollEvent> {
		let available = PollEvent::POLLOUT | PollEvent::POLLWRNORM | PollEvent::POLLWRBAND;
		Ok(event & available)
	}

	async fn write(&self, buf: &[u8]) -> io::Result<usize> {
		let write_params = WriteParams {
			fd: STDERR_FILENO,
			buf: GuestVirtAddr::from_ptr(buf.as_ptr()),
			len: buf.len(),
		};
		uhyve_hypercall(Hypercall::FileWrite(&write_params));

		Ok(write_params.len)
	}

	async fn isatty(&self) -> io::Result<bool> {
		Ok(true)
	}

	async fn fstat(&self) -> io::Result<FileAttr> {
		let attr = FileAttr {
			st_mode: AccessPermission::S_IFCHR,
			..Default::default()
		};
		Ok(attr)
	}
}

impl UhyveStderr {
	pub const fn new() -> Self {
		Self {}
	}
}
