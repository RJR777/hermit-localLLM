use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use memory_addresses::{PhysAddr, VirtAddr};
use pci_types::CommandRegister;
use usb_oxide::{MscDevice, UsbDevice, XhciCtrl};

use crate::arch::pci::PciConfigRegion;
use crate::drivers::pci::PciDevice;
use crate::init_cell::InitCell;
use crate::mm::PageRangeAllocator;

const USB_DEFAULT_DEVICE_BLOCK_SIZE: usize = 512;
const MIB: usize = 1024 * 1024;
const USB_MAX_READ_BYTES: usize = 4 * MIB;
const USB_INGEST_PROGRESS_INTERVAL: usize = 512 * MIB;
const RAW_GGUF_FALLBACK_SIZE: usize = 1_187_801_280;

#[derive(Clone, Copy, Debug)]
struct Fat32Volume {
	base_lba: u32,
	device_block_size: usize,
	bytes_per_sector: usize,
	device_blocks_per_fs_sector: u32,
	sectors_per_cluster: u32,
	reserved_sectors: u32,
	num_fats: u32,
	fat_size_sectors: u32,
	root_cluster: u32,
	data_start_lba: u32,
}

#[derive(Clone, Copy, Debug)]
struct Fat32File {
	first_cluster: u32,
	size: usize,
}

impl Fat32Volume {
	fn device_lba_for_fs_sector(&self, fs_sector: u32) -> u32 {
		self.base_lba + fs_sector * self.device_blocks_per_fs_sector
	}

	fn first_device_lba_of_cluster(&self, cluster: u32) -> u32 {
		self.device_lba_for_fs_sector(
			self.reserved_sectors
				+ self.num_fats * self.fat_size_sectors
				+ (cluster - 2) * self.sectors_per_cluster,
		)
	}

	fn fat_lba_for_cluster(&self, cluster: u32) -> (u32, usize) {
		let fat_offset = cluster * 4;
		let sector = self.device_lba_for_fs_sector(
			self.reserved_sectors + (fat_offset / self.bytes_per_sector as u32),
		);
		let offset = (fat_offset % self.bytes_per_sector as u32) as usize;
		(sector, offset)
	}
}

fn le_u16(buf: &[u8], off: usize) -> u16 {
	u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn le_u32(buf: &[u8], off: usize) -> u32 {
	u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn ascii_lower_u16(ch: u16) -> u16 {
	if (b'A' as u16..=b'Z' as u16).contains(&ch) {
		ch + 32
	} else {
		ch
	}
}

fn ascii_upper_u8(ch: u8) -> u8 {
	if ch.is_ascii_lowercase() { ch - 32 } else { ch }
}

fn fat32_entry_is_gguf(entry: &[u8]) -> bool {
	if entry[0] == 0x00 || entry[0] == 0xe5 {
		return false;
	}
	let attr = entry[11];
	if attr == 0x0f || (attr & 0x08) != 0 || (attr & 0x10) != 0 {
		return false;
	}
	&entry[8..11] == b"GGU" || &entry[8..11] == b"GGF"
}

fn fat32_entry_is_preferred_bitnet_gguf(entry: &[u8]) -> bool {
	if !fat32_entry_is_gguf(entry) {
		return false;
	}

	let prefix = b"GGML";
	entry[..prefix.len()]
		.iter()
		.zip(prefix.iter())
		.all(|(&actual, &expected)| ascii_upper_u8(actual) == expected)
}

fn fat32_lfn_entry_has_gguf_suffix(entry: &[u8]) -> bool {
	if entry[11] != 0x0f {
		return false;
	}

	let mut suffix = [0u16; 5];
	let mut suffix_len = 0usize;
	for off in [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30] {
		let ch = le_u16(entry, off);
		if ch == 0 || ch == 0xffff {
			break;
		}
		if suffix_len < suffix.len() {
			suffix[suffix_len] = ch;
			suffix_len += 1;
		} else {
			suffix.copy_within(1.., 0);
			suffix[4] = ch;
		}
	}

	suffix_len == 5
		&& suffix[0] == b'.' as u16
		&& ascii_lower_u16(suffix[1]) == b'g' as u16
		&& ascii_lower_u16(suffix[2]) == b'g' as u16
		&& ascii_lower_u16(suffix[3]) == b'u' as u16
		&& ascii_lower_u16(suffix[4]) == b'f' as u16
}

fn fat32_file_from_entry(entry: &[u8]) -> Option<Fat32File> {
	let attr = entry[11];
	if entry[0] == 0x00
		|| entry[0] == 0xe5
		|| attr == 0x0f
		|| (attr & 0x08) != 0
		|| (attr & 0x10) != 0
	{
		return None;
	}
	let first_cluster = ((le_u16(entry, 20) as u32) << 16) | le_u16(entry, 26) as u32;
	let size = le_u32(entry, 28) as usize;
	if first_cluster >= 2 && size > 0 {
		Some(Fat32File {
			first_cluster,
			size,
		})
	} else {
		None
	}
}

fn fat32_directory_from_entry(entry: &[u8]) -> Option<u32> {
	let attr = entry[11];
	if entry[0] == 0x00
		|| entry[0] == 0xe5
		|| entry[0] == b'.'
		|| attr == 0x0f
		|| (attr & 0x10) == 0
	{
		return None;
	}
	let first_cluster = ((le_u16(entry, 20) as u32) << 16) | le_u16(entry, 26) as u32;
	(first_cluster >= 2).then_some(first_cluster)
}

fn fat32_file_has_gguf_magic(
	msc: &mut MscDevice<HermitDma>,
	vol: Fat32Volume,
	file: Fat32File,
	sector_buf: &mut [u8],
) -> bool {
	let lba = vol.first_device_lba_of_cluster(file.first_cluster);
	read_device_sectors(
		msc,
		lba,
		vol.device_blocks_per_fs_sector as u16,
		&mut sector_buf[..vol.bytes_per_sector],
	) && &sector_buf[0..4] == b"GGUF"
}

fn parse_fat32_volume(
	base_lba: u32,
	device_block_size: usize,
	boot_sector: &[u8],
) -> Option<Fat32Volume> {
	if boot_sector.len() < 512 || boot_sector[510] != 0x55 || boot_sector[511] != 0xaa {
		return None;
	}
	let bytes_per_sector = le_u16(boot_sector, 11) as usize;
	let sectors_per_cluster = boot_sector[13] as u32;
	let reserved_sectors = le_u16(boot_sector, 14) as u32;
	let num_fats = boot_sector[16] as u32;
	let fat_size_sectors = le_u32(boot_sector, 36);
	let root_cluster = le_u32(boot_sector, 44);

	if bytes_per_sector == 0
		|| device_block_size == 0
		|| bytes_per_sector % device_block_size != 0
		|| sectors_per_cluster == 0
		|| reserved_sectors == 0
		|| num_fats == 0
		|| fat_size_sectors == 0
		|| root_cluster < 2
		|| &boot_sector[82..90] != b"FAT32   "
	{
		return None;
	}

	Some(Fat32Volume {
		base_lba,
		device_block_size,
		bytes_per_sector,
		device_blocks_per_fs_sector: (bytes_per_sector / device_block_size) as u32,
		sectors_per_cluster,
		reserved_sectors,
		num_fats,
		fat_size_sectors,
		root_cluster,
		data_start_lba: base_lba
			+ (reserved_sectors + num_fats * fat_size_sectors)
				* (bytes_per_sector / device_block_size) as u32,
	})
}

fn read_device_sectors(
	msc: &mut MscDevice<HermitDma>,
	lba: u32,
	device_blocks: u16,
	buf: &mut [u8],
) -> bool {
	if device_blocks > 1 {
		let bytes_per_block = buf.len() / device_blocks as usize;
		let split_blocks = device_blocks / 2;
		let split_bytes = split_blocks as usize * bytes_per_block;

		if read_device_sectors_once(msc, lba, device_blocks, buf) {
			return true;
		}

		warn!(
			"USB MSC: splitting read at LBA {} count {} into {} + {} blocks",
			lba,
			device_blocks,
			split_blocks,
			device_blocks - split_blocks
		);

		return read_device_sectors(msc, lba, split_blocks, &mut buf[..split_bytes])
			&& read_device_sectors(
				msc,
				lba + split_blocks as u32,
				device_blocks - split_blocks,
				&mut buf[split_bytes..],
			);
	}

	read_device_sectors_once(msc, lba, device_blocks, buf)
}

fn read_device_sectors_once(
	msc: &mut MscDevice<HermitDma>,
	lba: u32,
	device_blocks: u16,
	buf: &mut [u8],
) -> bool {
	for attempt in 0..3 {
		if msc.read_blocks(0, lba, device_blocks, buf).is_ok() {
			return true;
		}
		warn!(
			"USB MSC: read_blocks failed at LBA {} count {} attempt {}",
			lba,
			device_blocks,
			attempt + 1
		);
	}
	false
}

fn read_fat_entry(
	msc: &mut MscDevice<HermitDma>,
	vol: Fat32Volume,
	cluster: u32,
	sector_buf: &mut [u8],
) -> Option<u32> {
	let (fat_lba, offset) = vol.fat_lba_for_cluster(cluster);
	if !read_device_sectors(
		msc,
		fat_lba,
		vol.device_blocks_per_fs_sector as u16,
		&mut sector_buf[..vol.bytes_per_sector],
	) {
		return None;
	}
	Some(le_u32(sector_buf, offset) & 0x0fff_ffff)
}

fn find_gguf_file_in_fat32(
	msc: &mut MscDevice<HermitDma>,
	vol: Fat32Volume,
	cluster_buf: &mut [u8],
	sector_buf: &mut [u8],
) -> Option<Fat32File> {
	find_gguf_file_in_fat32_dir(msc, vol, vol.root_cluster, cluster_buf, sector_buf, 0)
}

fn find_gguf_file_in_fat32_dir(
	msc: &mut MscDevice<HermitDma>,
	vol: Fat32Volume,
	start_cluster: u32,
	cluster_buf: &mut [u8],
	sector_buf: &mut [u8],
	depth: usize,
) -> Option<Fat32File> {
	let cluster_bytes = vol.bytes_per_sector * vol.sectors_per_cluster as usize;
	let cluster_device_blocks = (cluster_bytes / vol.device_block_size) as u16;
	if cluster_buf.len() < cluster_bytes {
		return None;
	}

	let mut cluster = start_cluster;
	let mut clusters_seen = 0usize;
	let mut pending_lfn_is_gguf = false;
	let mut first_gguf_file: Option<Fat32File> = None;
	let mut largest_file: Option<Fat32File> = None;
	let mut subdirs = [0u32; 16];
	let mut subdir_count = 0usize;
	let mut end_of_directory = false;
	while (2..0x0fff_fff8).contains(&cluster) && clusters_seen < 4096 {
		let lba = vol.first_device_lba_of_cluster(cluster);
		if !read_device_sectors(
			msc,
			lba,
			cluster_device_blocks,
			&mut cluster_buf[..cluster_bytes],
		) {
			return None;
		}

		for entry in cluster_buf[..cluster_bytes].chunks_exact(32) {
			if entry[0] == 0x00 {
				end_of_directory = true;
				break;
			}
			if entry[11] == 0x0f {
				pending_lfn_is_gguf |= fat32_lfn_entry_has_gguf_suffix(entry);
				continue;
			}
			if let Some(file) = fat32_file_from_entry(entry) {
				if fat32_entry_is_preferred_bitnet_gguf(entry) {
					println!(
						"USB MSC: FAT32 found preferred BitNet GGUF file cluster {} size {} bytes",
						file.first_cluster, file.size
					);
					return Some(file);
				}
				if fat32_entry_is_gguf(entry) || pending_lfn_is_gguf {
					println!(
						"USB MSC: FAT32 found GGUF file cluster {} size {} bytes",
						file.first_cluster, file.size
					);
					if first_gguf_file.is_none() {
						first_gguf_file = Some(file);
					}
				}
				if fat32_file_has_gguf_magic(msc, vol, file, sector_buf) {
					println!(
						"USB MSC: FAT32 found GGUF magic file cluster {} size {} bytes",
						file.first_cluster, file.size
					);
					if first_gguf_file.is_none() {
						first_gguf_file = Some(file);
					}
				}
				if file.size > 1024 * 1024 * 1024
					&& largest_file.is_none_or(|current: Fat32File| file.size > current.size)
				{
					largest_file = Some(file);
				}
			} else if depth < 4
				&& subdir_count < subdirs.len()
				&& let Some(dir_cluster) = fat32_directory_from_entry(entry)
			{
				subdirs[subdir_count] = dir_cluster;
				subdir_count += 1;
			}
			pending_lfn_is_gguf = false;
		}

		if end_of_directory {
			break;
		}
		cluster = read_fat_entry(msc, vol, cluster, sector_buf)?;
		clusters_seen += 1;
	}
	if depth < 4 {
		for dir_cluster in subdirs[..subdir_count].iter().copied() {
			if let Some(file) = find_gguf_file_in_fat32_dir(
				msc,
				vol,
				dir_cluster,
				cluster_buf,
				sector_buf,
				depth + 1,
			) {
				return Some(file);
			}
		}
	}
	if let Some(file) = first_gguf_file {
		println!(
			"USB MSC: FAT32 using first GGUF fallback cluster {} size {} bytes",
			file.first_cluster, file.size
		);
		return Some(file);
	}
	if let Some(file) = largest_file {
		println!(
			"USB MSC: FAT32 using largest root file fallback cluster {} size {} bytes",
			file.first_cluster, file.size
		);
		return Some(file);
	}
	None
}

pub(crate) static XHCI_CONTROLLERS: InitCell<Vec<(Arc<XhciCtrl<HermitDma>>, usize)>> =
	InitCell::new(Vec::new());
pub(crate) static USB_WAKER: hermit_sync::InterruptTicketMutex<crate::executor::WakerRegistration> =
	hermit_sync::InterruptTicketMutex::new(crate::executor::WakerRegistration::new());
static XHCI_IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn handle_interrupt() {
	let _count = XHCI_IRQ_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
}

async fn yield_now() {
	struct YieldNow(bool);
	impl core::future::Future for YieldNow {
		type Output = ();
		fn poll(
			mut self: core::pin::Pin<&mut Self>,
			cx: &mut core::task::Context<'_>,
		) -> core::task::Poll<()> {
			if self.0 {
				core::task::Poll::Ready(())
			} else {
				self.0 = true;
				USB_WAKER.lock().register(cx.waker());
				cx.waker().wake_by_ref();
				core::task::Poll::Pending
			}
		}
	}
	YieldNow(false).await;
}

pub(crate) struct HermitDma;

unsafe impl Send for HermitDma {}
unsafe impl Sync for HermitDma {}

impl usb_oxide::Dma for HermitDma {
	unsafe fn alloc(&self, size: usize, align: usize) -> Option<usize> {
		// CRITICAL: Any multi-page DMA buffer (size > 4096) MUST be aligned to a 64KB physical boundary
		// to guarantee that xHCI TRBs never span across a 64KB page boundary!
		let align = if size > 4096 {
			align.max(65536)
		} else {
			align.max(4096)
		};
		let size = align_address::Align::align_up(size, align);
		let layout = free_list::PageLayout::from_size_align(size, align).ok()?;
		let frame = crate::mm::FrameAlloc::allocate(layout).ok()?;
		let phys_addr = PhysAddr::new(frame.start() as u64);
		let virt_addr =
			VirtAddr::from_ptr(crate::mm::device_alloc::DeviceAlloc.ptr_from::<u8>(phys_addr));
		unsafe {
			core::ptr::write_bytes(virt_addr.as_mut_ptr::<u8>(), 0, size);
		}
		Some(virt_addr.as_u64() as usize)
	}

	unsafe fn free(&self, addr: usize, size: usize, _align: usize) {
		let align = if size > 4096 {
			_align.max(65536)
		} else {
			_align.max(4096)
		};
		let size = align_address::Align::align_up(size, align);
		let p_start = self.virt_to_phys(addr);
		let range = free_list::PageRange::new(p_start, p_start + size).unwrap();
		unsafe { crate::mm::FrameAlloc::deallocate(range) };
	}

	unsafe fn map_mmio(&self, phys: usize, size: usize) -> Option<usize> {
		let virtual_address = crate::mm::map(
			PhysAddr::new(phys as u64),
			size,
			true, // writable
			true, // no_execution
			true, // no_cache
		);
		Some(virtual_address.as_u64() as usize)
	}

	unsafe fn unmap_mmio(&self, virt: usize, size: usize) {
		crate::mm::unmap(VirtAddr::new(virt as u64), size);
	}

	fn virt_to_phys(&self, va: usize) -> usize {
		let ptr = core::ptr::with_exposed_provenance_mut::<u8>(va as usize);
		crate::mm::device_alloc::DeviceAlloc
			.phys_addr_from(ptr)
			.as_u64() as usize
	}
}

pub fn init_device(device: &PciDevice<PciConfigRegion>) {
	init_device_with_irq(device, 0);
}

pub fn init_device_with_irq(device: &PciDevice<PciConfigRegion>, _irq: u8) {
	println!("xHCI: DISCOVERED device at {}", device.address());

	let bar = device.get_bar(0).expect("No xHCI BAR0 found");
	println!("xHCI: BAR0 is {:?}", bar);
	let phys_addr = match bar {
		pci_types::Bar::Memory32 { address, .. } => address as u64,
		pci_types::Bar::Memory64 { address, .. } => address,
		_ => panic!("xHCI BAR0 is not memory!"),
	};

	// Enable PCI bus mastering and memory space
	device.set_command(CommandRegister::BUS_MASTER_ENABLE | CommandRegister::MEMORY_ENABLE);

	println!("xHCI: Creating XhciCtrl...");
	let ctrl =
		Arc::new(XhciCtrl::new(phys_addr as usize, HermitDma).expect("Failed to create XhciCtrl"));
	println!("xHCI: XhciCtrl created successfully");

	XHCI_CONTROLLERS.with(|v| v.unwrap().push((ctrl.clone(), 0)));

	crate::executor::spawn(async move {
		info!("xHCI: Handler task started");

		if let Some(info) = crate::env::model_info() {
			println!(
				"USB xHCI: Model already loaded from {} @ {:#x} ({} MiB), skipping USB MSC ingestion",
				info.source,
				info.address,
				info.size / MIB
			);
			loop {
				ctrl.poll();
				yield_now().await;
			}
		}

		// PRE-ALLOCATION: Vacuum up all low-memory frames using 4KB allocations.
		// The previous 1MB discards were skipping over small, fragmented holes below 1MB
		// where 0x37000 was hiding. By allocating 8,192 individual 4KB pages (32MB total),
		// we guarantee every legacy frame is consumed before the bounce buffer is created.
		let mut discard_addrs = Vec::new();
		let discard_layout = core::alloc::Layout::from_size_align(4096, 4096).unwrap();
		for _ in 0..8192 {
			if let Ok(ptr) = core::alloc::Allocator::allocate(
				&crate::mm::device_alloc::DeviceAlloc,
				discard_layout,
			) {
				discard_addrs.push(ptr.as_mut_ptr().addr() as u64);
			}
		}
		core::hint::black_box(&discard_addrs);
		println!("USB MSC: Permanently reserved 64MB of low-memory for hardware stability.");

		let mut msc_found = false;
		loop {
			ctrl.poll();

			if !msc_found {
				if let Some(info) = crate::env::model_info() {
					if info.address != 0 {
						println!(
							"USB xHCI: Model became available from {} @ {:#x}, skipping USB search",
							info.source, info.address
						);
						msc_found = true;
					}
				}

				if msc_found {
					yield_now().await;
					continue;
				}

				for p in 0..ctrl.max_ports() {
					if !ctrl.port_connected(p) {
						continue;
					}
					println!(
						"USB xHCI: Found connected device on Port {}! Initializing...",
						p
					);
					if let Ok(usb_dev) = UsbDevice::new(ctrl.clone(), p) {
						if let Ok(mut msc) = MscDevice::new(Arc::new(usb_dev)) {
							println!("USB MSC: Mass storage device active on Port {}!", p);

							msc_found = true;

							let mut device_block_size = USB_DEFAULT_DEVICE_BLOCK_SIZE;
							if let Ok(capacity) = msc.read_capacity(0) {
								device_block_size = capacity.block_size() as usize;
								println!(
									"USB MSC: Device Capacity: {} sectors, Sector Size: {} bytes",
									capacity.last_lba() + 1,
									capacity.block_size()
								);
							}

							if let Ok(inquiry) = msc.inquiry(0) {
								let vendor =
									core::str::from_utf8(&inquiry.vendor).unwrap_or("Unknown");
								let product =
									core::str::from_utf8(&inquiry.product).unwrap_or("Unknown");
								println!(
									"USB MSC: Vendor: {}, Product: {}",
									vendor.trim(),
									product.trim()
								);
							}

							// Allocate a simple 4KB bounce buffer strictly for querying the partition sector headers
							let bounce_layout =
								core::alloc::Layout::from_size_align(4096, 4096).unwrap();
							let mut bounce_buf = unsafe {
								let ptr = core::alloc::Allocator::allocate(
									&crate::mm::device_alloc::DeviceAlloc,
									bounce_layout,
								)
								.unwrap();
								core::slice::from_raw_parts_mut(ptr.as_mut_ptr(), 4096)
							};

							// Step 2: Discover the FAT32 partition dynamically from the MBR.
							let mut part_start = 0;
							let mut part_type = 0;
							println!("USB MSC: Reading Master Boot Record (LBA 0)...");
							if read_device_sectors(
								&mut msc,
								0,
								1,
								&mut bounce_buf[..device_block_size],
							) {
								// Partition 1 sits at byte offset 446
								part_type = bounce_buf[446 + 4];
								part_start = le_u32(&bounce_buf, 446 + 8);
								println!(
									"USB MSC: MBR Partition 1 Type: {:#x}, Start LBA: {}",
									part_type, part_start
								);
							}

							let mut candidates = Vec::new();
							if matches!(part_type, 0x0b | 0x0c) && part_start > 0 {
								candidates.push(part_start);
							}
							candidates.push(2048);

							let mut fat_volume = None;
							let mut fat_file = None;
							let mut raw_start_lba = None;
							for base_lba in candidates {
								println!("USB MSC: Probing LBA {}...", base_lba);
								if !read_device_sectors(
									&mut msc,
									base_lba,
									1,
									&mut bounce_buf[..device_block_size],
								) {
									continue;
								}
								println!(
									"USB MSC: LBA {} header: {:02x?}",
									base_lba,
									&bounce_buf[0..16]
								);
								if &bounce_buf[0..4] == b"GGUF" {
									raw_start_lba = Some(base_lba);
									break;
								}
								if let Some(vol) = parse_fat32_volume(
									base_lba,
									device_block_size,
									&bounce_buf[..device_block_size],
								) {
									println!(
										"USB MSC: FAT32 at LBA {}. bytes/sector {}, sectors/cluster {}, reserved {}, FATs {}, FAT size {}, root cluster {}, data LBA {}",
										base_lba,
										vol.bytes_per_sector,
										vol.sectors_per_cluster,
										vol.reserved_sectors,
										vol.num_fats,
										vol.fat_size_sectors,
										vol.root_cluster,
										vol.data_start_lba
									);
									let cluster_bytes =
										vol.bytes_per_sector * vol.sectors_per_cluster as usize;
									let mut cluster_buf = alloc::vec![0u8; cluster_bytes];
									if let Some(file) = find_gguf_file_in_fat32(
										&mut msc,
										vol,
										&mut cluster_buf,
										&mut bounce_buf,
									) {
										fat_volume = Some(vol);
										fat_file = Some(file);
										break;
									}
								}
							}

							let model_size =
								fat_file.map(|f| f.size).unwrap_or(RAW_GGUF_FALLBACK_SIZE);

							// Step 4: Allocate memory from global heap
							println!(
								"USB MSC: Allocating {} bytes from global heap for model buffer...",
								model_size
							);
							let model_alloc_size =
								align_address::Align::align_up(model_size, USB_MAX_READ_BYTES);
							let model_layout =
								core::alloc::Layout::from_size_align(model_alloc_size, 4096)
									.unwrap();
							// To keep the future Send, we store the address as a usize rather than keeping the raw pointer across await points
							let target_base = unsafe {
								let ptr = alloc::alloc::alloc(model_layout);
								if ptr.is_null() {
									0
								} else {
									ptr.expose_provenance()
								}
							};
							if target_base == 0 {
								error!(
									"USB MSC: FATAL: Failed to allocate {} bytes from global heap!",
									model_size
								);
								break;
							}
							println!(
								"USB MSC: Successfully allocated global heap buffer @ {:#x} (4 KB aligned)",
								target_base
							);

							// Step 5: Ingest the file. FAT32 follows the cluster chain; raw fallback keeps the old signature-scan behavior alive.
							let mut loaded = false;
							let model_load_start_us = crate::arch::processor::get_timer_ticks();
							if let (Some(vol), Some(file)) = (fat_volume, fat_file) {
								println!(
									"USB MSC: Ingesting FAT32 GGUF cluster chain from cluster {} into heap @ {:#x}",
									file.first_cluster, target_base
								);
								let cluster_bytes =
									vol.bytes_per_sector * vol.sectors_per_cluster as usize;
								let cluster_device_blocks =
									(cluster_bytes / vol.device_block_size) as u16;
								let mut cluster = file.first_cluster;
								let mut copied = 0usize;
								let mut next_progress = USB_INGEST_PROGRESS_INTERVAL;
								let mut failure_reason = None;
								while copied < file.size && (2..0x0fff_fff8).contains(&cluster) {
									let run_start_cluster = cluster;
									let mut run_clusters = 1usize;
									let mut next_cluster =
										read_fat_entry(&mut msc, vol, cluster, &mut bounce_buf);
									while run_clusters * cluster_bytes < USB_MAX_READ_BYTES {
										match next_cluster {
											Some(next)
												if next == cluster + 1
													&& copied + run_clusters * cluster_bytes
														< file.size =>
											{
												cluster = next;
												run_clusters += 1;
												next_cluster = read_fat_entry(
													&mut msc,
													vol,
													cluster,
													&mut bounce_buf,
												);
											}
											_ => break,
										}
									}
									let run_bytes = run_clusters * cluster_bytes;
									let target_ptr = core::ptr::with_exposed_provenance_mut::<u8>(
										target_base + copied,
									);
									let target_slice = unsafe {
										core::slice::from_raw_parts_mut(target_ptr, run_bytes)
									};
									if !read_device_sectors(
										&mut msc,
										vol.first_device_lba_of_cluster(run_start_cluster),
										(cluster_device_blocks as usize * run_clusters) as u16,
										target_slice,
									) {
										error!(
											"USB MSC: FATAL read error at FAT32 cluster {}",
											run_start_cluster
										);
										failure_reason = Some("read error");
										break;
									}
									copied = (copied + run_bytes).min(file.size);
									if copied >= next_progress || copied == file.size {
										print_usb_ingest_progress(copied, file.size);
										while next_progress <= copied {
											next_progress += USB_INGEST_PROGRESS_INTERVAL;
										}
										yield_now().await;
									}
									if copied < file.size {
										match next_cluster {
											Some(next) => cluster = next,
											None => {
												failure_reason =
													Some("FAT chain ended before file size");
												break;
											}
										}
									}
								}
								loaded = copied >= file.size;
								if loaded {
									println!(
										"USB MSC: FAT32 ingestion copied {} bytes (directory size {})",
										copied, file.size
									);
								} else {
									if copied > 0
										&& failure_reason.is_none() && cluster >= 0x0fff_fff8
									{
										let missing = file.size - copied;
										error!(
											"USB MSC: FAT32 chain ended at EOC cluster {:#x}; refusing truncated copy {} ({} bytes short of directory size {})",
											cluster, copied, missing, file.size
										);
									}
									error!(
										"USB MSC: FAT32 ingestion incomplete: copied {} / {} bytes, cluster {}, reason {}",
										copied,
										file.size,
										cluster,
										failure_reason.unwrap_or("invalid cluster or unknown")
									);
								}
							} else if let Some(start_lba) = raw_start_lba {
								let burst_bytes =
									USB_MAX_READ_BYTES.min(u16::MAX as usize * device_block_size);
								let burst_blocks = burst_bytes / device_block_size;
								let total_blocks =
									(model_size + device_block_size - 1) / device_block_size;
								println!(
									"USB MSC: Ingesting raw GGUF {} blocks from LBA {} into heap @ {:#x}",
									total_blocks, start_lba, target_base
								);
								let mut blocks_done = 0usize;
								let mut next_progress = USB_INGEST_PROGRESS_INTERVAL;
								let mut failure_lba = None;
								while blocks_done < total_blocks {
									let count = (total_blocks - blocks_done).min(burst_blocks);
									let target_ptr = core::ptr::with_exposed_provenance_mut::<u8>(
										target_base + blocks_done * device_block_size,
									);
									let target_slice = unsafe {
										core::slice::from_raw_parts_mut(
											target_ptr,
											count * device_block_size,
										)
									};
									if !read_device_sectors(
										&mut msc,
										start_lba + blocks_done as u32,
										count as u16,
										target_slice,
									) {
										error!(
											"USB MSC: FATAL read error at raw LBA {}",
											start_lba + blocks_done as u32
										);
										failure_lba = Some(start_lba + blocks_done as u32);
										break;
									}
									blocks_done += count;
									let bytes_done = blocks_done * device_block_size;
									if bytes_done >= next_progress || blocks_done >= total_blocks {
										print_usb_ingest_progress(
											bytes_done.min(model_size),
											model_size,
										);
										while next_progress <= bytes_done {
											next_progress += USB_INGEST_PROGRESS_INTERVAL;
										}
										yield_now().await;
									}
								}
								loaded = blocks_done >= total_blocks;
								if loaded {
									println!(
										"USB MSC: Raw ingestion copied {} / {} bytes",
										(blocks_done * device_block_size).min(model_size),
										model_size
									);
								} else {
									error!(
										"USB MSC: Raw ingestion incomplete: copied {} / {} bytes, blocks {} / {}, failed_lba {:?}",
										(blocks_done * device_block_size).min(model_size),
										model_size,
										blocks_done,
										total_blocks,
										failure_lba
									);
								}
							} else {
								error!(
									"USB MSC: GGUF file was not found on FAT32 and no raw GGUF signature was found"
								);
							}

							if loaded {
								println!("USB MSC: Ingestion COMPLETE!");
								let model_source = if fat_file.is_some() {
									"usb-msc-fat32"
								} else {
									"usb-msc-raw"
								};
								crate::env::set_model_info_with_source(
									target_base,
									model_size,
									model_source,
								);
								let model_load_us = crate::arch::processor::get_timer_ticks()
									.saturating_sub(model_load_start_us);
								let mib_per_s = if model_load_us > 0 {
									(model_size as u64).saturating_mul(1_000_000)
										/ model_load_us / MIB as u64
								} else {
									0
								};
								println!(
									"PERF model_load source={} bytes={} elapsed_us={} throughput_mib_s={} address={:#x}",
									model_source, model_size, model_load_us, mib_per_s, target_base
								);
								let model_ptr =
									core::ptr::with_exposed_provenance::<u8>(target_base);
								if crate::drivers::storage::ahci::write_raw_model_cache(
									model_ptr, model_size,
								)
								.is_err()
								{
									error!("USB MSC: failed to write AHCI raw model cache");
								}
							} else {
								error!("USB MSC: Ingestion FAILED");
							}
							yield_now().await;
							break;
						}
					}
				}
			}

			yield_now().await;
		}
	});
}

fn print_usb_ingest_progress(done: usize, total: usize) {
	println!(
		"[{} us] USB MSC: Ingestion Progress {}/{} MiB",
		crate::arch::processor::get_timer_ticks(),
		done / MIB,
		total / MIB
	);
}
