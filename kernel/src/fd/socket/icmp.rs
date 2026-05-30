use alloc::boxed::Box;
use core::mem::MaybeUninit;
use core::task::Poll;
use core::time::Duration;
use core::{future, slice};

use async_trait::async_trait;
use smoltcp::socket::icmp;
use smoltcp::wire::{IpEndpoint, Ipv4Address, Ipv6Address};

use crate::errno::Errno;
use crate::executor::block_on;
use crate::executor::network::{Handle, NETWORK_WAKER, NIC};
use crate::fd::{self, Endpoint, ListenEndpoint, ObjectInterface, PollEvent, SocketOption};
use crate::io;
use crate::syscalls::socket::Af;

pub struct Socket {
	handle: Handle,
	nonblocking: bool,
	local_endpoint: IpEndpoint,
	remote_endpoint: Option<IpEndpoint>,
	read_timeout: Option<Duration>,
	write_timeout: Option<Duration>,
}

impl Socket {
	pub fn new(handle: Handle, domain: Af) -> Self {
		let local_endpoint = if domain == Af::Inet {
			IpEndpoint::new(Ipv4Address::UNSPECIFIED.into(), 0)
		} else if domain == Af::Inet6 {
			IpEndpoint::new(Ipv6Address::UNSPECIFIED.into(), 0)
		} else {
			panic!("Unsupported domain for ICMP socket: {domain:?}");
		};

		let socket = Self {
			handle,
			nonblocking: false,
			local_endpoint,
			remote_endpoint: None,
			read_timeout: None,
			write_timeout: None,
		};

		socket.with(|socket| {
			let _ = socket.bind(icmp::Endpoint::Unspecified);
		});

		socket
	}

	fn with<R>(&self, f: impl FnOnce(&mut icmp::Socket<'_>) -> R) -> R {
		let mut guard = NIC.lock();
		let nic = guard.as_nic_mut().unwrap();
		f(nic.get_mut_socket::<icmp::Socket<'_>>(self.handle))
	}

	async fn close(&self) -> io::Result<()> {
		// ICMP sockets don't really have a close handshake
		Ok(())
	}
}

#[async_trait]
impl ObjectInterface for Socket {
	async fn poll(&self, event: PollEvent) -> io::Result<PollEvent> {
		future::poll_fn(|cx| {
			self.with(|socket| {
				let ret = if socket.is_open() {
					let mut avail = PollEvent::empty();

					if socket.can_send() {
						avail.insert(
							PollEvent::POLLOUT | PollEvent::POLLWRNORM | PollEvent::POLLWRBAND,
						);
					}

					if socket.can_recv() {
						avail.insert(
							PollEvent::POLLIN | PollEvent::POLLRDNORM | PollEvent::POLLRDBAND,
						);
					}

					event & avail
				} else {
					PollEvent::POLLNVAL
				};

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

	async fn bind(&mut self, endpoint: ListenEndpoint) -> io::Result<()> {
		#[allow(irrefutable_let_patterns)]
		let ListenEndpoint::Ip(endpoint) = endpoint else {
			return Err(Errno::Io);
		};

		self.local_endpoint.port = endpoint.port;
		if let Some(addr) = endpoint.addr {
			self.local_endpoint.addr = addr;
		}

		let icmp_endpoint = if endpoint.port != 0 {
			icmp::Endpoint::Ident(endpoint.port)
		} else {
			icmp::Endpoint::Unspecified
		};

		self.with(|socket| socket.bind(icmp_endpoint).map_err(|_| Errno::Addrinuse))
	}

	async fn connect(&mut self, endpoint: Endpoint) -> io::Result<()> {
		#[allow(irrefutable_let_patterns)]
		let Endpoint::Ip(endpoint) = endpoint else {
			return Err(Errno::Io);
		};

		self.remote_endpoint = Some(endpoint);
		Ok(())
	}

	async fn sendto(&self, buf: &[u8], endpoint: Endpoint) -> io::Result<usize> {
		#[allow(irrefutable_let_patterns)]
		let Endpoint::Ip(endpoint) = endpoint else {
			return Err(Errno::Io);
		};

		future::poll_fn(|cx| {
			self.with(|socket| {
				if socket.can_send() {
					debug!("icmp: sending {} bytes to {}", buf.len(), endpoint.addr);
					let res = Poll::Ready(
						socket
							.send_slice(buf, endpoint.addr)
							.map(|()| buf.len())
							.map_err(|_| Errno::Io),
					);
					NETWORK_WAKER.lock().wake();
					res
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
					let buffer = unsafe {
						slice::from_raw_parts_mut(buffer.as_mut_ptr() as *mut u8, buffer.len())
					};
					match socket.recv_slice(buffer) {
						Ok((len, addr)) => {
							debug!("icmp: received {} bytes from {}", len, addr);
							Poll::Ready(Ok((len, Endpoint::Ip(IpEndpoint::new(addr, 0)))))
						}
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
		future::poll_fn(|cx| {
			self.with(|socket| {
				if socket.can_recv() {
					match socket.recv_slice(buffer) {
						Ok((len, _addr)) => Poll::Ready(Ok(len)),
						Err(_) => {
							socket.register_recv_waker(cx.waker());
							Poll::<io::Result<usize>>::Pending
						}
					}
				} else {
					socket.register_recv_waker(cx.waker());
					Poll::<io::Result<usize>>::Pending
				}
			})
		})
		.await
	}

	async fn write(&self, buf: &[u8]) -> io::Result<usize> {
		let endpoint = self.remote_endpoint.ok_or(Errno::Inval)?;
		self.sendto(buf, Endpoint::Ip(endpoint)).await
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
