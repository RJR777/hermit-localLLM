use alloc::boxed::Box;
use core::future;
use core::mem::MaybeUninit;
use core::task::Poll;
use core::time::Duration;

use async_trait::async_trait;
use smoltcp::socket::raw;
use smoltcp::wire::{IpEndpoint, IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr};

use crate::errno::Errno;
use crate::executor::block_on;
use crate::executor::network::{Handle, NETWORK_WAKER, NIC};
use crate::fd::{self, Endpoint, ListenEndpoint, ObjectInterface, PollEvent, SocketOption};
use crate::io;

pub struct Socket {
	handle: Handle,
	nonblocking: bool,
	protocol: IpProtocol,
	read_timeout: Option<Duration>,
	write_timeout: Option<Duration>,
}

impl Socket {
	pub fn new(handle: Handle, protocol: IpProtocol) -> Self {
		Self {
			handle,
			nonblocking: false,
			protocol,
			read_timeout: None,
			write_timeout: None,
		}
	}

	fn with<R>(&self, f: impl FnOnce(&mut raw::Socket<'_>) -> R) -> R {
		let mut guard = NIC.lock();
		let nic = guard.as_nic_mut().unwrap();
		f(nic.get_mut_socket::<raw::Socket<'_>>(self.handle))
	}

	async fn close(&self) -> io::Result<()> {
		Ok(())
	}
}

#[async_trait]
impl ObjectInterface for Socket {
	async fn poll(&self, event: PollEvent) -> io::Result<PollEvent> {
		future::poll_fn(|cx| {
			self.with(|socket| {
				let mut avail = PollEvent::empty();

				if socket.can_send() {
					avail
						.insert(PollEvent::POLLOUT | PollEvent::POLLWRNORM | PollEvent::POLLWRBAND);
				}

				if socket.can_recv() {
					avail.insert(PollEvent::POLLIN | PollEvent::POLLRDNORM | PollEvent::POLLRDBAND);
				}

				let ret = event & avail;

				if ret.is_empty() {
					if event.intersects(
						PollEvent::POLLIN | PollEvent::POLLRDNORM | PollEvent::POLLRDBAND,
					) {
						socket.register_recv_waker(cx.waker());
					}

					if event.intersects(
						PollEvent::POLLOUT | PollEvent::POLLWRNORM | PollEvent::POLLWRBAND,
					) {
						socket.register_send_waker(cx.waker());
					}

					Poll::Pending
				} else {
					Poll::Ready(Ok(ret))
				}
			})
		})
		.await
	}

	async fn bind(&mut self, _endpoint: ListenEndpoint) -> io::Result<()> {
		Ok(())
	}

	async fn connect(&mut self, _endpoint: Endpoint) -> io::Result<()> {
		Ok(())
	}

	async fn sendto(&self, buf: &[u8], endpoint: Endpoint) -> io::Result<usize> {
		#[allow(irrefutable_let_patterns)]
		let Endpoint::Ip(endpoint) = endpoint else {
			return Err(Errno::Io);
		};

		let smoltcp_addr = endpoint.addr;
		let protocol = self.protocol;

		future::poll_fn(|cx| {
			self.with(|socket| {
				if socket.can_send() {
					let mut guard = NIC.lock();
					let nic = guard.as_nic_mut().unwrap();
					let src_addr = nic.ipv4_addr().unwrap_or(Ipv4Address::UNSPECIFIED);
					let caps = nic.capabilities();
					drop(guard);

					match smoltcp_addr {
						smoltcp::wire::IpAddress::Ipv4(dst_addr) => {
							let repr = Ipv4Repr {
								src_addr,
								dst_addr,
								next_header: protocol,
								payload_len: buf.len(),
								hop_limit: 64,
							};

							let mut full_packet = vec![0u8; repr.buffer_len() + buf.len()];
							{
								let mut packet = Ipv4Packet::new_unchecked(&mut full_packet);
								repr.emit(&mut packet, &caps.checksum);
								packet.payload_mut().copy_from_slice(buf);
							}

							info!(
								"raw: sending {} bytes from {} to {} (proto={})",
								buf.len(),
								src_addr,
								dst_addr,
								protocol
							);
							info!(
								"raw: send payload head: {:02x?}",
								&buf[..core::cmp::min(buf.len(), 16)]
							);

							let res = Poll::Ready(
								socket
									.send_slice(&full_packet)
									.map(|()| buf.len())
									.map_err(|_| Errno::Io),
							);
							NETWORK_WAKER.lock().wake();
							res
						}
						_ => Poll::Ready(Err(Errno::Inval)),
					}
				} else {
					socket.register_send_waker(cx.waker());
					Poll::<io::Result<usize>>::Pending
				}
			})
		})
		.await
	}

	async fn recvfrom(&self, buffer: &mut [MaybeUninit<u8>]) -> io::Result<(usize, Endpoint)> {
		future::poll_fn(|cx| {
			self.with(|socket| {
				if socket.can_recv() {
					let mut temp_buf = vec![0u8; buffer.len() + 60];
					match socket.recv_slice(&mut temp_buf) {
						Ok(len) => match Ipv4Packet::new_checked(&temp_buf[..len]) {
							Ok(packet) => {
								let header_len = packet.header_len() as usize;
								let payload = &temp_buf[header_len..len];
								let to_copy = core::cmp::min(payload.len(), buffer.len());

								info!(
									"raw: received {} bytes from {} (proto={})",
									payload.len(),
									packet.src_addr(),
									packet.next_header()
								);
								info!(
									"raw: received payload: {:02x?}",
									&payload[..core::cmp::min(payload.len(), 16)]
								);

								unsafe {
									core::ptr::copy_nonoverlapping(
										payload.as_ptr(),
										buffer.as_mut_ptr() as *mut u8,
										to_copy,
									);
								}

								let src_addr = packet.src_addr();
								Poll::Ready(Ok((
									to_copy,
									Endpoint::Ip(IpEndpoint::new(src_addr.into(), 0)),
								)))
							}
							Err(e) => {
								trace!("raw: received non-IPv4 or malformed packet: {:?}", e);
								socket.register_recv_waker(cx.waker());
								Poll::<io::Result<(usize, Endpoint)>>::Pending
							}
						},
						Err(_) => {
							socket.register_recv_waker(cx.waker());
							Poll::<io::Result<(usize, Endpoint)>>::Pending
						}
					}
				} else {
					socket.register_recv_waker(cx.waker());
					Poll::<io::Result<(usize, Endpoint)>>::Pending
				}
			})
		})
		.await
	}

	async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
		let mut temp_buf = vec![MaybeUninit::uninit(); buffer.len()];
		let (len, _) = self.recvfrom(&mut temp_buf).await?;
		unsafe {
			core::ptr::copy_nonoverlapping(
				temp_buf.as_ptr() as *const u8,
				buffer.as_mut_ptr(),
				len,
			);
		}
		Ok(len)
	}

	async fn write(&self, _buf: &[u8]) -> io::Result<usize> {
		Err(Errno::Inval)
	}

	async fn status_flags(&self) -> io::Result<fd::StatusFlags> {
		let status_flags = if self.nonblocking {
			fd::StatusFlags::O_NONBLOCK
		} else {
			fd::StatusFlags::empty()
		};

		Ok(status_flags)
	}

	async fn read_timeout(&self) -> Option<Duration> {
		self.read_timeout
	}

	async fn write_timeout(&self) -> Option<Duration> {
		self.write_timeout
	}

	async fn set_status_flags(&mut self, status_flags: fd::StatusFlags) -> io::Result<()> {
		self.nonblocking = status_flags.contains(fd::StatusFlags::O_NONBLOCK);
		Ok(())
	}

	async fn setsockopt(&mut self, opt: SocketOption) -> io::Result<()> {
		match opt {
			SocketOption::ReadTimeout(timeout) => self.read_timeout = timeout,
			SocketOption::WriteTimeout(timeout) => self.write_timeout = timeout,
			_ => return Err(Errno::Inval),
		}
		Ok(())
	}

	async fn getsockopt(&mut self, opt: SocketOption) -> io::Result<SocketOption> {
		match opt {
			SocketOption::ReadTimeout(_) => Ok(SocketOption::ReadTimeout(self.read_timeout)),
			SocketOption::WriteTimeout(_) => Ok(SocketOption::WriteTimeout(self.write_timeout)),
			_ => Err(Errno::Inval),
		}
	}
}

impl Drop for Socket {
	fn drop(&mut self) {
		let _ = block_on(self.close(), None);
		NIC.lock().as_nic_mut().unwrap().destroy_socket(self.handle);
	}
}
