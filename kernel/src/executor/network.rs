use alloc::boxed::Box;
#[cfg(feature = "dns")]
use alloc::vec::Vec;
use core::future;
use core::sync::atomic::{AtomicU16, Ordering};
use core::task::Poll;

use hermit_sync::InterruptTicketMutex;
use smoltcp::iface::{PollResult, SocketHandle};
use smoltcp::phy::Device;
#[cfg(feature = "dns")]
use smoltcp::socket::dns::{self, GetQueryResultError, QueryHandle};
#[cfg(feature = "tcp")]
use smoltcp::socket::tcp;
#[cfg(feature = "udp")]
use smoltcp::socket::udp;
use smoltcp::socket::{AnySocket, dhcpv4, icmp, raw};
use smoltcp::time::{Duration, Instant};
#[cfg(feature = "dns")]
use smoltcp::wire::{DnsQueryType, IpAddress};
#[cfg(feature = "dhcpv4")]
use smoltcp::wire::{IpCidr, Ipv4Address, Ipv4Cidr};

use crate::arch;
use crate::drivers::net::{NetworkDevice, NetworkDriver};
#[cfg(feature = "dns")]
use crate::errno::Errno;
use crate::executor::{WakerRegistration, spawn};
use crate::scheduler::PerCoreSchedulerExt;

pub(crate) static NETWORK_WAKER: InterruptTicketMutex<WakerRegistration> =
	InterruptTicketMutex::new(WakerRegistration::new());

pub(crate) enum NetworkState<'a> {
	Missing,
	// Never constructed if the kernel is configured for the loopback driver.
	#[allow(dead_code)]
	InitializationFailed,
	Initialized(Box<NetworkInterface<'a>>),
}

impl<'a> NetworkState<'a> {
	pub fn as_nic_mut(&mut self) -> Result<&mut NetworkInterface<'a>, &'static str> {
		match self {
			NetworkState::Initialized(nic) => Ok(nic),
			_ => Err("Network is not initialized"),
		}
	}
}

pub(crate) type Handle = SocketHandle;

pub(crate) static NIC: InterruptTicketMutex<NetworkState<'_>> =
	InterruptTicketMutex::new(NetworkState::Missing);

pub(crate) struct NetworkInterface<'a> {
	pub(crate) iface: smoltcp::iface::Interface,
	pub(crate) sockets: smoltcp::iface::SocketSet<'a>,
	pub(crate) device: NetworkDevice,
	#[cfg(feature = "dhcpv4")]
	pub(crate) dhcp_handle: SocketHandle,
	#[cfg(feature = "dns")]
	pub(crate) dns_handle: Option<SocketHandle>,
}

pub(crate) fn network_handler() {
	let mut guard = NIC.lock();
	if let Ok(nic) = guard.as_nic_mut() {
		#[cfg(feature = "net-trace")]
		nic.device.get_mut().handle_interrupt();
		#[cfg(not(feature = "net-trace"))]
		nic.device.handle_interrupt();
	}

	NETWORK_WAKER.lock().wake();
}

static LOCAL_ENDPOINT: AtomicU16 = AtomicU16::new(0);

fn start_endpoint() -> u16 {
	((arch::kernel::systemtime::now_micros() % (u16::MAX as u64 - 49152 + 1)) + 49152)
		.try_into()
		.unwrap()
}

pub(crate) fn get_ephemeral_port() -> u16 {
	LOCAL_ENDPOINT
		.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |port| {
			Some(if port < 65535 { port + 1 } else { 49152 })
		})
		.unwrap()
}

pub(crate) fn now() -> Instant {
	Instant::from_micros(i64::try_from(arch::processor::get_timer_ticks()).unwrap())
}

async fn network_executor_task() {
	let mut poll_counter = 0u32;
	future::poll_fn(|cx| {
		let mut guard = NIC.lock();
		let NetworkState::Initialized(nic) = &mut *guard else {
			return Poll::Ready(());
		};

		poll_counter = poll_counter.wrapping_add(1);
		let time = now();

		// 1. Poll the interface (processes packets and background socket logic)
		if nic.poll_common(time) == PollResult::SocketStateChanged {
			trace!("network: packet processed");
		}

		// 2. Handle DHCP events if enabled
		#[cfg(feature = "dhcpv4")]
		{
			let dhcp_handle = nic.dhcp_handle;
			let socket = nic.sockets.get_mut::<dhcpv4::Socket<'_>>(dhcp_handle);
			match socket.poll() {
				None => {}
				Some(dhcpv4::Event::Configured(config)) => {
					info!("DHCP config acquired!");
					info!("IP address:   {}", config.address);
					nic.iface.update_ip_addrs(|addrs| {
						if let Some(dest) = addrs.iter_mut().next() {
							*dest = IpCidr::Ipv4(config.address);
						} else if addrs.push(IpCidr::Ipv4(config.address)).is_err() {
							info!("Unable to update IP address");
						}
					});
					if let Some(router) = config.router {
						info!("Gateway:      {router}");
						nic.iface
							.routes_mut()
							.add_default_ipv4_route(router)
							.unwrap();
					} else {
						info!("Gateway:      None");
						nic.iface.routes_mut().remove_default_ipv4_route();
					}

					#[cfg(feature = "dns")]
					{
						let mut dns_servers: Vec<IpAddress> = Vec::new();
						for (i, s) in config.dns_servers.iter().enumerate() {
							info!("DNS server {i}: {s}");
							dns_servers.push(IpAddress::Ipv4(*s));
						}
						if !dns_servers.is_empty() {
							if let Some(dns_handle) = nic.dns_handle {
								nic.sockets.remove(dns_handle);
							}
							let dns_socket = dns::Socket::new(dns_servers.as_slice(), vec![]);
							nic.dns_handle = Some(nic.sockets.add(dns_socket));
						}
					}
				}
				Some(dhcpv4::Event::Deconfigured) => {
					// Only log if we were previously configured
					if !nic
						.iface
						.ipv4_addr()
						.unwrap_or(Ipv4Address::UNSPECIFIED)
						.is_unspecified()
					{
						info!("DHCP lost config!");
						let cidr = Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0);
						nic.iface.update_ip_addrs(|addrs| {
							if let Some(dest) = addrs.iter_mut().next() {
								*dest = IpCidr::Ipv4(cidr);
							}
						});
						nic.iface.routes_mut().remove_default_ipv4_route();

						#[cfg(feature = "dns")]
						{
							if let Some(dns_handle) = nic.dns_handle {
								nic.sockets.remove(dns_handle);
							}
							nic.dns_handle = None;
						}
					}
				}
			}
		}

		// 3. Schedule next wakeup
		let wakeup_time = nic
			.poll_delay(time)
			.map(|d| crate::arch::processor::get_timer_ticks() + d.total_micros());
		crate::core_scheduler().add_network_timer(wakeup_time);

		// 4. Register for wakeup
		NETWORK_WAKER.lock().register(cx.waker());

		Poll::<()>::Pending
	})
	.await;
}

#[cfg(feature = "dns")]
pub(crate) async fn get_query_result(query: QueryHandle) -> io::Result<Vec<IpAddress>> {
	future::poll_fn(|cx| {
		let mut guard = NIC.lock();
		let nic = guard.as_nic_mut().unwrap();
		let dns_socket = nic.get_mut_dns_socket().unwrap();

		match dns_socket.get_query_result(query) {
			Ok(addrs) => Poll::Ready(Ok(addrs.to_vec())),
			Err(GetQueryResultError::Pending) => {
				dns_socket.register_query_waker(query, cx.waker());
				Poll::Pending
			}
			Err(e) => {
				warn!("DNS query failed: {e:?}");
				Poll::Ready(Err(Errno::Noent))
			}
		}
	})
	.await
}

pub(crate) fn init() {
	info!("Try to initialize network!");

	// initialize variable, which contains the next local endpoint
	LOCAL_ENDPOINT.store(start_endpoint(), Ordering::Relaxed);

	let mut guard = NIC.lock();

	*guard = NetworkInterface::create();

	if let NetworkState::Initialized(_) = *guard {
		drop(guard);
		spawn(network_executor_task());
	}
}

impl<'a> NetworkInterface<'a> {
	#[cfg(feature = "udp")]
	pub(crate) fn create_udp_handle(&mut self) -> Result<Handle, ()> {
		let udp_rx_buffer =
			udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 65535]);
		let udp_tx_buffer =
			udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 65535]);
		let udp_socket = udp::Socket::new(udp_rx_buffer, udp_tx_buffer);
		let udp_handle = self.sockets.add(udp_socket);

		Ok(udp_handle)
	}

	pub(crate) fn create_raw_handle(
		&mut self,
		protocol: smoltcp::wire::IpProtocol,
	) -> Result<Handle, ()> {
		let raw_rx_buffer =
			raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 16], vec![0; 0x4000]);
		let raw_tx_buffer =
			raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 16], vec![0; 0x4000]);
		let raw_socket = raw::Socket::new(
			smoltcp::wire::IpVersion::Ipv4,
			protocol,
			raw_rx_buffer,
			raw_tx_buffer,
		);
		let raw_handle = self.sockets.add(raw_socket);

		Ok(raw_handle)
	}

	#[cfg(feature = "icmp")]
	pub(crate) fn create_icmp_handle(&mut self) -> Result<Handle, ()> {
		let icmp_rx_buffer =
			icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 16], vec![0; 0x4000]);
		let icmp_tx_buffer =
			icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 16], vec![0; 0x4000]);
		let icmp_socket = icmp::Socket::new(icmp_rx_buffer, icmp_tx_buffer);
		let icmp_handle = self.sockets.add(icmp_socket);

		Ok(icmp_handle)
	}

	#[cfg(feature = "tcp")]
	pub(crate) fn create_tcp_handle(&mut self) -> Result<Handle, ()> {
		let tcp_rx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
		let tcp_tx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
		let tcp_socket = tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer);
		let tcp_handle = self.sockets.add(tcp_socket);

		Ok(tcp_handle)
	}

	pub(crate) fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
		self.device.capabilities()
	}

	pub(crate) fn ipv4_addr(&self) -> Option<Ipv4Address> {
		self.iface.ipv4_addr()
	}

	pub(crate) fn poll_common(&mut self, timestamp: Instant) -> PollResult {
		self.iface
			.poll(timestamp, &mut self.device, &mut self.sockets)
	}

	pub(crate) fn poll_delay(&mut self, timestamp: Instant) -> Option<Duration> {
		self.iface.poll_delay(timestamp, &mut self.sockets)
	}

	pub(crate) fn get_mut_socket<T: AnySocket<'a>>(&mut self, handle: SocketHandle) -> &mut T {
		self.sockets.get_mut::<T>(handle)
	}

	pub(crate) fn get_socket_and_context<T: AnySocket<'a>>(
		&mut self,
		handle: SocketHandle,
	) -> (&mut T, &mut smoltcp::iface::Context) {
		(self.sockets.get_mut::<T>(handle), self.iface.context())
	}

	pub(crate) fn destroy_socket(&mut self, handle: Handle) {
		self.sockets.remove(handle);
	}

	#[cfg(feature = "dns")]
	pub(crate) fn start_query(
		&mut self,
		name: &str,
		query_type: DnsQueryType,
	) -> Result<QueryHandle, dns::StartQueryError> {
		let dns_socket = self.get_mut_dns_socket().unwrap();
		dns_socket.start_query(self.iface.context(), name, query_type)
	}

	#[cfg(feature = "dns")]
	pub(crate) fn get_mut_dns_socket(&mut self) -> io::Result<&mut dns::Socket<'a>> {
		self.dns_handle
			.ok_or(Errno::Eio)
			.map(|handle| self.sockets.get_mut::<dns::Socket<'a>>(handle))
	}

	pub(crate) fn set_polling_mode(&mut self, value: bool) {
		#[cfg(feature = "net-trace")]
		self.device.get_mut().set_polling_mode(value);
		#[cfg(not(feature = "net-trace"))]
		self.device.set_polling_mode(value);
	}
}
