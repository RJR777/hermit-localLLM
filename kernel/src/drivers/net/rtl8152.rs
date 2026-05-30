use alloc::collections::vec_deque::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use hermit_sync::SpinMutex;
use smoltcp::phy;
use smoltcp::time::Instant;
use usb_oxide::{Dma, PhysMem, Rtl8152Device};

use crate::drivers::net::NetworkDriver;
use crate::drivers::{Driver, InterruptLine};
use crate::mm::device_alloc::DeviceAlloc;

const RTL8152_MTU: usize = 1536;

pub(crate) struct Rtl8152NetworkDriver<H: Dma> {
	device: Arc<Rtl8152Device<H>>,
	rx_queue: SpinMutex<VecDeque<Vec<u8, DeviceAlloc>>>,
	reserved_receives: AtomicUsize,
	rx_buf: Option<PhysMem<H>>,
	rx_pending_len: usize,
}

impl<H: Dma> Rtl8152NetworkDriver<H> {
	pub(crate) fn new(device: Arc<Rtl8152Device<H>>) -> Self {
		// Add a settle delay to ensure XHCI handoff is complete
		crate::arch::processor::udelay(10_000);

		let mut driver = Self {
			device,
			rx_queue: SpinMutex::new(VecDeque::new()),
			reserved_receives: AtomicUsize::new(0),
			rx_buf: None,
			rx_pending_len: Rtl8152Device::<H>::rx_buffer_len(RTL8152_MTU),
		};
		driver.arm_receive();
		driver
	}

	fn arm_receive(&mut self) {
		if self.rx_buf.is_some() {
			return;
		}

		let host = self.device.usb_device().ctrl().host();
		match PhysMem::alloc(host, self.rx_pending_len, 8) {
			Ok(buf) => {
				if let Err(err) = self.device.queue_rx_packet(&buf, self.rx_pending_len) {
					error!("RTL8153: failed to queue RX transfer: {:?}", err);
					buf.free(host);
					return;
				}
				self.rx_buf = Some(buf);
			}
			Err(err) => error!("RTL8153: failed to allocate RX buffer: {:?}", err),
		}
	}

	fn poll_rx(&mut self) {
		loop {
			let Some(buf) = self.rx_buf.as_ref() else {
				self.arm_receive();
				return;
			};

			let mut frame = Vec::with_capacity_in(RTL8152_MTU, DeviceAlloc);
			frame.resize(RTL8152_MTU, 0);

			match self
				.device
				.poll_rx_packet(buf, self.rx_pending_len, &mut frame)
			{
				Some(Ok(len)) => {
					trace!("rtl8152: received packet len={}", len);
					frame.truncate(len);
					self.rx_queue.lock().push_back(frame);

					let old = self.rx_buf.take().unwrap();
					let host = self.device.usb_device().ctrl().host();
					old.free(host);
					self.arm_receive();
				}
				Some(Err(err)) => {
					error!("RTL8153: RX transfer failed: {:?}", err);
					let old = self.rx_buf.take().unwrap();
					let host = self.device.usb_device().ctrl().host();
					old.free(host);
					self.arm_receive();
				}
				None => break,
			}
		}
	}
}

impl<H: Dma> Driver for Rtl8152NetworkDriver<H> {
	fn get_interrupt_number(&self) -> InterruptLine {
		0xff
	}

	fn get_name(&self) -> &'static str {
		"rtl8153"
	}
}

pub(crate) struct TxToken<'a, H: Dma> {
	device: &'a Arc<Rtl8152Device<H>>,
}

impl<H: Dma> smoltcp::phy::TxToken for TxToken<'_, H> {
	fn consume<R, F>(self, len: usize, f: F) -> R
	where
		F: FnOnce(&mut [u8]) -> R,
	{
		let mut buffer = Vec::with_capacity_in(len, DeviceAlloc);
		buffer.resize(len, 0);
		let result = f(&mut buffer);
		trace!("rtl8152: sending packet len={}", len);
		if let Err(err) = self.device.write_packet(&buffer) {
			error!("RTL8153: TX failed: {:?}", err);
		}
		result
	}
}

pub(crate) struct RxToken<'a> {
	queue: &'a SpinMutex<VecDeque<Vec<u8, DeviceAlloc>>>,
	reserved_receives: &'a AtomicUsize,
}

impl smoltcp::phy::RxToken for RxToken<'_> {
	fn consume<R, F>(self, f: F) -> R
	where
		F: FnOnce(&[u8]) -> R,
	{
		let frame = self.queue.lock().pop_front();
		f(&frame.unwrap())
	}
}

impl Drop for RxToken<'_> {
	fn drop(&mut self) {
		self.reserved_receives.fetch_sub(1, Ordering::Relaxed);
	}
}

impl<H: Dma> smoltcp::phy::Device for Rtl8152NetworkDriver<H> {
	type RxToken<'a>
		= RxToken<'a>
	where
		H: 'a;
	type TxToken<'a>
		= TxToken<'a, H>
	where
		H: 'a;

	fn receive(&mut self, _: Instant) -> Option<(RxToken<'_>, TxToken<'_, H>)> {
		self.poll_rx();

		if self.rx_queue.lock().len() <= self.reserved_receives.load(Ordering::Relaxed) {
			return None;
		}

		self.reserved_receives.fetch_add(1, Ordering::Relaxed);
		Some((
			RxToken {
				queue: &self.rx_queue,
				reserved_receives: &self.reserved_receives,
			},
			TxToken {
				device: &self.device,
			},
		))
	}

	fn transmit(&mut self, _: Instant) -> Option<TxToken<'_, H>> {
		Some(TxToken {
			device: &self.device,
		})
	}

	fn capabilities(&self) -> phy::DeviceCapabilities {
		let mut capabilities = phy::DeviceCapabilities::default();
		capabilities.medium = phy::Medium::Ethernet;
		capabilities.max_transmission_unit = RTL8152_MTU;
		capabilities.checksum.ipv4 = phy::Checksum::Both;
		capabilities.checksum.udp = phy::Checksum::Both;
		capabilities.checksum.tcp = phy::Checksum::Both;
		capabilities.checksum.icmpv4 = phy::Checksum::Both;
		capabilities.checksum.icmpv6 = phy::Checksum::Both;
		capabilities
	}
}

impl<H: Dma> NetworkDriver for Rtl8152NetworkDriver<H> {
	fn get_mac_address(&self) -> [u8; 6] {
		self.device.mac_address()
	}

	fn has_packet(&self) -> bool {
		!self.rx_queue.lock().is_empty()
	}

	fn set_polling_mode(&mut self, _value: bool) {}

	fn handle_interrupt(&mut self) {
		self.poll_rx();
	}
}

pub(crate) type NetworkDevice = Rtl8152NetworkDriver<crate::drivers::usb::xhci::HermitDma>;
