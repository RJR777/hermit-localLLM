use alloc::boxed::Box;
use alloc::string::String;
use core::alloc::Layout;
use core::slice;

use hermit_sync::OnceCell;
use pci_types::CommandRegister;

use crate::arch::kernel::core_local::core_scheduler;
use crate::arch::pci::PciConfigRegion;
use crate::drivers::pci::PciDevice;
use crate::env;
use crate::mm::device_alloc::DeviceAlloc;
use crate::mm::{self, PhysAddr, VirtAddr};
use crate::scheduler::PerCoreSchedulerExt;

static FIRST_SATA_PORT: OnceCell<usize> = OnceCell::new();
static FIRST_SATA_PORT_CL: OnceCell<usize> = OnceCell::new();
static FIRST_SATA_PORT_FIS: OnceCell<usize> = OnceCell::new();
static MODEL_CACHE_START_LBA: OnceCell<u64> = OnceCell::new();
static SSD_SECTOR_COUNT: OnceCell<u64> = OnceCell::new();
static MEMORY_DB_HEADER_LBA: OnceCell<u64> = OnceCell::new();

const SECTOR_SIZE: usize = 512;
const MODEL_FIXED_CACHE_LBA: u64 = 2048;
const MODEL_HEADER_SECTORS: u64 = 8;
const MODEL_IO_SECTORS: u16 = 8192;
const MODEL_LOAD_PROGRESS_INTERVAL: usize = 128 * 1024 * 1024;
const MIN_RAW_MODEL_SIZE: usize = 1024 * 1024 * 1024;
const MAX_RAW_MODEL_SIZE: usize = 8 * 1024 * 1024 * 1024;
const MODEL_MAGIC_READY: &[u8; 8] = b"BITNET01";
const MODEL_MAGIC_WRITING: &[u8; 8] = b"BITNET00";
const MODEL_CACHE_TAG: &[u8; 8] = b"BITNET2B";
const MODEL_CACHE_TAG_OFFSET: usize = 24;
const MEMORY_DB_MAGIC: &[u8; 8] = b"MEMDB002";
const MEMORY_DB_VERSION: u32 = 2;
const MEMORY_DB_TOTAL_SECTORS: u64 = 131_072; // 64 MiB at 512 bytes/sector.
const MEMORY_DB_MAP_MAGIC: &[u8; 4] = b"DMAP";
const MEMORY_DB_MAP_VERSION: u32 = 1;
const MEMORY_DB_MAP_SECTORS: u64 = 32; // 16 KiB: 1 header + 31 entry sectors.
const MEMORY_DB_MAP_ENTRY_SIZE: usize = 16;
const MEMORY_DB_MAP_ENTRY_SIZE_U32: u32 = 16;
const MEMORY_DB_MAP_ENTRIES_PER_SECTOR: u64 =
	(SECTOR_SIZE as u64) / MEMORY_DB_MAP_ENTRY_SIZE as u64;
const MEMORY_DB_MAP_MAX_ENTRIES: u64 =
	(MEMORY_DB_MAP_SECTORS.saturating_sub(1)) * MEMORY_DB_MAP_ENTRIES_PER_SECTOR;

const MEMORY_DB_VERSION_OFFSET: usize = 8;
const MEMORY_DB_SECTOR_SIZE_OFFSET: usize = 12;
const MEMORY_DB_HEADER_LBA_OFFSET: usize = 16;
const MEMORY_DB_DATA_LBA_OFFSET: usize = 24;
const MEMORY_DB_TOTAL_SECTORS_OFFSET: usize = 32;
const MEMORY_DB_NEXT_APPEND_LBA_OFFSET: usize = 40;
const MEMORY_DB_LAST_SSD_LBA_OFFSET: usize = 48;
const MEMORY_DB_MODEL_START_LBA_OFFSET: usize = 56;
const MEMORY_DB_MODEL_LAST_LBA_OFFSET: usize = 64;
const MEMORY_DB_MAP_LBA_OFFSET: usize = 72;
const MEMORY_DB_MAP_ENTRIES_USED_OFFSET: usize = 80;
const MEMORY_DB_MAP_SECTORS_OFFSET: usize = 88;
const MEMORY_DB_MAP_HEADER_MAGIC_OFFSET: usize = 0;
const MEMORY_DB_MAP_HEADER_VERSION_OFFSET: usize = 4;
const MEMORY_DB_MAP_HEADER_ENTRY_SIZE_OFFSET: usize = 8;
const MEMORY_DB_MAP_HEADER_CAPACITY_OFFSET: usize = 12;
const MEMORY_DB_MAP_HEADER_USED_OFFSET: usize = 16;
const MEMORY_DB_MAP_ENTRY_START_OFFSET: usize = 0;
const MEMORY_DB_MAP_ENTRY_BYTES_OFFSET: usize = 8;
const MEMORY_DB_MAP_ENTRY_FLAGS_OFFSET: usize = 12;
const MEMORY_DB_MAP_FLAG_ACTIVE: u32 = 1 << 0;
const MEMORY_DB_ERASE_CHUNK_SECTORS: u16 = 8;
const MEMORY_DB_ERASE_CHUNK_BYTES: usize = MEMORY_DB_ERASE_CHUNK_SECTORS as usize * SECTOR_SIZE;

#[derive(Clone, Copy)]
struct MemoryDbHeader {
	magic: [u8; 8],
	version: u32,
	sector_size: u32,
	header_lba: u64,
	data_lba: u64,
	total_sectors: u64,
	next_append_lba: u64,
	last_ssd_lba: u64,
	model_start_lba: u64,
	model_last_lba: u64,
	map_lba: u64,
	map_entries_used: u64,
	map_sectors: u64,
}

#[derive(Clone, Copy, Default)]
struct MemoryDbMapEntry {
	start_lba: u64,
	payload_bytes: u32,
	flags: u32,
}

#[repr(C, align(32))]
#[derive(Clone, Copy, Default)]
struct CommandHeader {
	cfl_flags: u16,
	prdtl: u16,
	prdbc: u32,
	ctba: u32,
	ctbau: u32,
	reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PRDTEntry {
	dba: u32,
	dbau: u32,
	reserved: u32,
	dbc_flags: u32,
}

#[repr(C, align(128))]
struct CommandTable {
	cfis: [u8; 64],
	acmd: [u8; 16],
	reserved: [u8; 48],
	prdt: [PRDTEntry; 1],
}

pub(crate) fn init() {
	// AHCI initialization is currently handled by PCI enumeration discovery.
}

pub(crate) fn enumerate_controller(device: &PciDevice<PciConfigRegion>) {
	// Enable Bus Mastering and Memory Space
	device.set_command(CommandRegister::BUS_MASTER_ENABLE | CommandRegister::MEMORY_ENABLE);

	let Some((base, size)) = device.memory_map_bar(5, true) else {
		warn!(
			"Cannot enumerate AHCI controller at {}: BAR5 (ABAR) is not MMIO-mappable",
			device.address()
		);
		return;
	};

	let ghc = base.as_ptr::<u32>() as *mut u32;

	// 1. Enable AHCI mode first (required before reset on some HBAs)
	unsafe {
		let mut ghc_val = ghc.add(1).read_volatile();
		ghc_val |= 1 << 31; // AE (AHCI Enable)
		ghc.add(1).write_volatile(ghc_val);
	}

	// 2. Perform HBA Reset
	unsafe {
		ghc.add(1).write_volatile(ghc.add(1).read_volatile() | 1); // GHC.HR = 1
		for _ in 0..1000 {
			if ghc.add(1).read_volatile() & 1 == 0 {
				break;
			}
			crate::arch::processor::udelay(1000);
		}
	}

	// 3. Re-enable AHCI mode after reset
	unsafe {
		let mut ghc_val = ghc.add(1).read_volatile();
		ghc_val |= 1 << 31; // AE (AHCI Enable)
		ghc.add(1).write_volatile(ghc_val);
	}

	let cap = unsafe { ghc.read_volatile() };
	let pi = unsafe { ghc.add(3).read_volatile() };
	let vs = unsafe { ghc.add(4).read_volatile() };
	let ghc_reg = unsafe { ghc.add(1).read_volatile() };

	info!(
		"AHCI controller at {} (ABAR at {:#x}, size {:#x})",
		device.address(),
		base.as_u64(),
		size
	);
	info!(
		"AHCI version {}.{}.{}, capabilities {:#x}, GHC {:#x}, ports implemented {:#x}",
		(vs >> 16) & 0xffff,
		(vs >> 8) & 0xff,
		vs & 0xff,
		cap,
		ghc_reg,
		pi
	);

	for i in 0..32 {
		if pi & (1 << i) == 0 {
			continue;
		}

		let port_base = unsafe { base.as_ptr::<u8>().add(0x100 + i * 0x80) as *mut u32 };

		// Check if device is present before doing expensive stuff
		let ssts = unsafe { port_base.add(10).read_volatile() };
		if ssts & 0xf == 0 {
			continue;
		}

		// 3. Explicitly start port (Spin-Up and Power-On)
		unsafe {
			let mut cmd = port_base.add(6).read_volatile(); // PxCMD
			cmd |= (1 << 1) | (1 << 2); // SUD (Spin-Up Device) and POD (Power On Device)
			port_base.add(6).write_volatile(cmd);
		}

		// 4. Perform SATA COMRESET (Commented out per user request)
		/* unsafe {
			let mut sctl = port_base.add(11).read_volatile(); // PxSCTL
			sctl = (sctl & !0xf) | 1; // DET = 1 (Perform COMRESET)
			port_base.add(11).write_volatile(sctl);
			crate::arch::processor::udelay(1000);
			sctl &= !0xf; // DET = 0 (Normal operation)
			port_base.add(11).write_volatile(sctl);
		} */

		// Wait up to 100ms for device detection (reduced from 1s)
		let mut ssts = unsafe { port_base.add(10).read_volatile() };
		let mut det = ssts & 0xf;
		if det != 3 {
			for _ in 0..100 {
				crate::arch::processor::udelay(1000);
				ssts = unsafe { port_base.add(10).read_volatile() };
				det = ssts & 0xf;
				if det == 3 {
					break;
				}
			}
		}

		let ipm = (ssts >> 8) & 0xf;

		if det != 3 {
			continue;
		}

		info!("AHCI port {i}: DET={det}, IPM={ipm}, PxSSTS={ssts:#x}");

		// 5. Enable FIS Receive and Start port to get signature
		unsafe {
			let mut cmd = port_base.add(6).read_volatile(); // PxCMD
			cmd |= 1 << 4; // FRE (FIS Receive Enable)
			port_base.add(6).write_volatile(cmd);
			cmd |= 1; // ST (Start)
			port_base.add(6).write_volatile(cmd);
		}

		// Wait up to 500ms for signature to be valid (reduced from 2s)
		let mut sig = unsafe { port_base.add(9).read_volatile() };
		let mut device_type = ahci_device_type(sig);

		if device_type == "Unknown" {
			for _ in 0..500 {
				crate::arch::processor::udelay(1000);
				sig = unsafe { port_base.add(9).read_volatile() };
				device_type = ahci_device_type(sig);
				if device_type != "Unknown" {
					break;
				}
			}
		}

		info!("AHCI port {i}: {device_type} device detected, signature {sig:#x}");

		if device_type == "SATA" {
			if i == 2 {
				let cl = Box::new_in([CommandHeader::default(); 32], DeviceAlloc);
				let cl_virt = cl.as_ptr() as usize;
				let cl_phys = mm::virtual_to_physical(VirtAddr::from_ptr(cl.as_ptr()))
					.unwrap()
					.as_u64();

				let fis = Box::new_in([0u8; 256], DeviceAlloc);
				let fis_virt = fis.as_ptr() as usize;
				let fis_phys = mm::virtual_to_physical(VirtAddr::from_ptr(fis.as_ptr()))
					.unwrap()
					.as_u64();

				unsafe {
					stop_port(port_base);
					port_base.add(0).write_volatile(cl_phys as u32);
					port_base.add(1).write_volatile((cl_phys >> 32) as u32);
					port_base.add(2).write_volatile(fis_phys as u32);
					port_base.add(3).write_volatile((fis_phys >> 32) as u32);
					start_port(port_base);
				}

				Box::leak(cl);
				Box::leak(fis);

				let _ = FIRST_SATA_PORT.set(port_base as usize);
				let _ = FIRST_SATA_PORT_CL.set(cl_virt);
				let _ = FIRST_SATA_PORT_FIS.set(fis_virt);

				info!("AHCI port {i}: Starting identify_device...");
				identify_device(port_base, cl_virt as *mut CommandHeader);
				info!("AHCI port {i}: identify_device completed.");
				info!("AHCI port {i}: attempting raw model cache load from SSD");
				if try_load_raw_model() {
					info!("AHCI port {i}: raw model cache load succeeded");
				} else {
					warn!(
						"AHCI port {i}: raw model cache load unavailable; USB ingestion may continue"
					);
				}
			}
		}
	}
}

fn stop_port(port: *mut u32) {
	unsafe {
		let mut cmd = port.add(6).read_volatile();
		cmd &= !1; // ST = 0
		cmd &= !(1 << 4); // FRE = 0
		port.add(6).write_volatile(cmd);

		let mut timeout = 1000;
		while port.add(6).read_volatile() & ((1 << 15) | (1 << 14)) != 0 && timeout > 0 {
			timeout -= 1;
			crate::arch::processor::udelay(100);
		}

		if timeout == 0 {
			warn!("AHCI: Port failed to stop. (COMRESET fallback commented out)");
			/* warn!("AHCI: Port failed to stop. Issuing COMRESET...");
			// Issue COMRESET via PxSCTL (Offset 0x2C)
			let mut sctl = port.add(11).read_volatile();
			sctl = (sctl & !0x0F) | 1; // DET = 1 (Perform interface reset)
			port.add(11).write_volatile(sctl);
			crate::arch::processor::udelay(1000);
			sctl &= !0x0F; // DET = 0 (Resume communication)
			port.add(11).write_volatile(sctl);

			// Wait for communication to be re-established (PxSSTS DET should be 3)
			let mut reset_timeout = 1000;
			while (port.add(10).read_volatile() & 0x0F) != 3 && reset_timeout > 0 {
				reset_timeout -= 1;
				crate::arch::processor::udelay(100);
			} */
		}
	}
}

fn start_port(port: *mut u32) {
	unsafe {
		let mut timeout = 1000;
		while port.add(6).read_volatile() & (1 << 15) != 0 && timeout > 0 {
			timeout -= 1;
			crate::arch::processor::udelay(100);
		}
		if timeout == 0 {
			warn!("AHCI: Port failed to clear CR bit before start");
		}
		let mut cmd = port.add(6).read_volatile();
		cmd |= 1 << 4; // FRE = 1
		port.add(6).write_volatile(cmd);
		cmd |= 1; // ST = 1
		port.add(6).write_volatile(cmd);
	}
}

pub fn read_sectors(lba: u64, count: u16, buf_virt: *mut u8) -> Result<(), ()> {
	let Some(&port_addr) = FIRST_SATA_PORT.get() else {
		return Err(());
	};
	let port = port_addr as *mut u32;
	transfer_sectors(port, lba, count, buf_virt, false)
}

pub fn write_sectors(lba: u64, count: u16, buf_virt: *const u8) -> Result<(), ()> {
	let Some(&port_addr) = FIRST_SATA_PORT.get() else {
		return Err(());
	};
	let port = port_addr as *mut u32;
	transfer_sectors(port, lba, count, buf_virt as *mut u8, true)
}

fn sectors_for_bytes(bytes: usize) -> u64 {
	bytes.div_ceil(SECTOR_SIZE) as u64
}

fn log_raw_model_lba_range(context: &str, data_lba: u64, model_size: usize) {
	let sector_count = sectors_for_bytes(model_size);
	let end_lba = data_lba + sector_count.saturating_sub(1);
	let next_free_lba = data_lba + sector_count;
	info!(
		"AHCI raw model cache: {context} model bytes {}, sectors {}, LBA {}..{} inclusive, next free LBA {}",
		model_size, sector_count, data_lba, end_lba, next_free_lba
	);
}

fn raw_model_range_fits(data_lba: u64, model_size: usize) -> bool {
	let Some(&capacity_sectors) = SSD_SECTOR_COUNT.get() else {
		warn!("AHCI raw model cache: SSD capacity unknown; allowing raw range unchecked");
		return true;
	};
	let sector_count = sectors_for_bytes(model_size);
	let Some(end_exclusive) = data_lba.checked_add(sector_count) else {
		error!("AHCI raw model cache: raw range overflow");
		return false;
	};
	if end_exclusive > capacity_sectors {
		error!(
			"AHCI raw model cache: raw range LBA {}..{} exceeds SSD last LBA {} ({} sectors total)",
			data_lba,
			end_exclusive.saturating_sub(1),
			capacity_sectors.saturating_sub(1),
			capacity_sectors
		);
		return false;
	}
	true
}

fn write_le_u32(buf: &mut [u8], offset: usize, value: u32) {
	buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_le_u64(buf: &mut [u8], offset: usize, value: u64) {
	buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn memory_db_map_entry_capacity() -> u64 {
	MEMORY_DB_MAP_MAX_ENTRIES
}

fn memory_db_map_entry_location(entry_index: u64) -> Result<(u64, usize), ()> {
	if entry_index >= memory_db_map_entry_capacity() {
		return Err(());
	}

	let entries_per_sector = MEMORY_DB_MAP_ENTRIES_PER_SECTOR;
	let sector_offset = entry_index / entries_per_sector;
	let offset_within_sector =
		(entry_index % entries_per_sector).saturating_mul(MEMORY_DB_MAP_ENTRY_SIZE as u64);
	let entry_offset = usize::try_from(offset_within_sector).map_err(|_| ())?;
	Ok((sector_offset, entry_offset))
}

fn memory_db_read_map_header(map_lba: u64) -> Result<(u64, u64), ()> {
	let mut map_header = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	read_sectors(map_lba, 1, map_header.as_mut_ptr())?;

	if &map_header[MEMORY_DB_MAP_HEADER_MAGIC_OFFSET
		..MEMORY_DB_MAP_HEADER_MAGIC_OFFSET + MEMORY_DB_MAP_MAGIC.len()]
		!= MEMORY_DB_MAP_MAGIC
	{
		return Err(());
	}
	if le_u32(map_header.as_slice(), MEMORY_DB_MAP_HEADER_VERSION_OFFSET) != MEMORY_DB_MAP_VERSION {
		return Err(());
	}
	let entry_size = le_u32(
		map_header.as_slice(),
		MEMORY_DB_MAP_HEADER_ENTRY_SIZE_OFFSET,
	);
	if entry_size as u64 != MEMORY_DB_MAP_ENTRY_SIZE as u64 {
		return Err(());
	}

	let entry_capacity = le_u32(map_header.as_slice(), MEMORY_DB_MAP_HEADER_CAPACITY_OFFSET) as u64;
	let map_entries_used = le_u64(map_header.as_slice(), MEMORY_DB_MAP_HEADER_USED_OFFSET);
	if entry_capacity != memory_db_map_entry_capacity() || map_entries_used > entry_capacity {
		return Err(());
	}

	Ok((entry_size as u64, map_entries_used))
}

fn memory_db_write_map_header(map_lba: u64, map_entries_used: u64) -> Result<(), ()> {
	let mut map_header = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	map_header[MEMORY_DB_MAP_HEADER_MAGIC_OFFSET
		..MEMORY_DB_MAP_HEADER_MAGIC_OFFSET + MEMORY_DB_MAP_MAGIC.len()]
		.copy_from_slice(MEMORY_DB_MAP_MAGIC);
	write_le_u32(
		map_header.as_mut_slice(),
		MEMORY_DB_MAP_HEADER_VERSION_OFFSET,
		MEMORY_DB_MAP_VERSION,
	);
	write_le_u32(
		map_header.as_mut_slice(),
		MEMORY_DB_MAP_HEADER_ENTRY_SIZE_OFFSET,
		MEMORY_DB_MAP_ENTRY_SIZE_U32,
	);
	write_le_u32(
		map_header.as_mut_slice(),
		MEMORY_DB_MAP_HEADER_CAPACITY_OFFSET,
		u32::try_from(memory_db_map_entry_capacity()).map_err(|_| ())?,
	);
	write_le_u64(
		map_header.as_mut_slice(),
		MEMORY_DB_MAP_HEADER_USED_OFFSET,
		map_entries_used,
	);
	write_sectors(map_lba, 1, map_header.as_ptr())
}

fn memory_db_read_map_entry(map_lba: u64, entry_index: u64) -> Result<MemoryDbMapEntry, ()> {
	let (entry_sector_offset, entry_offset) = memory_db_map_entry_location(entry_index)?;
	let entry_lba = map_lba.saturating_add(entry_sector_offset + 1);
	let mut sector = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	read_sectors(entry_lba, 1, sector.as_mut_ptr())?;

	let entry = MemoryDbMapEntry {
		start_lba: le_u64(
			sector.as_slice(),
			entry_offset + MEMORY_DB_MAP_ENTRY_START_OFFSET,
		),
		payload_bytes: le_u32(
			sector.as_slice(),
			entry_offset + MEMORY_DB_MAP_ENTRY_BYTES_OFFSET,
		),
		flags: le_u32(
			sector.as_slice(),
			entry_offset + MEMORY_DB_MAP_ENTRY_FLAGS_OFFSET,
		),
	};
	Ok(entry)
}

fn memory_db_write_map_entry(
	map_lba: u64,
	entry_index: u64,
	start_lba: u64,
	payload_bytes: u32,
	flags: u32,
) -> Result<(), ()> {
	let (entry_sector_offset, entry_offset) = memory_db_map_entry_location(entry_index)?;
	let entry_lba = map_lba.saturating_add(entry_sector_offset + 1);
	let mut sector = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	read_sectors(entry_lba, 1, sector.as_mut_ptr())?;

	write_le_u64(
		sector.as_mut_slice(),
		entry_offset + MEMORY_DB_MAP_ENTRY_START_OFFSET,
		start_lba,
	);
	write_le_u32(
		sector.as_mut_slice(),
		entry_offset + MEMORY_DB_MAP_ENTRY_BYTES_OFFSET,
		payload_bytes,
	);
	write_le_u32(
		sector.as_mut_slice(),
		entry_offset + MEMORY_DB_MAP_ENTRY_FLAGS_OFFSET,
		flags,
	);
	write_sectors(entry_lba, 1, sector.as_ptr())
}

fn memory_db_register_state(db_lba: u64, data_lba: u64) {
	let _ = MEMORY_DB_HEADER_LBA.set(db_lba);
	let map_lba = db_lba.saturating_add(1);
	info!(
		"AHCI memory DB: registered header LBA {db_lba}, map LBA {map_lba}, and data LBA {data_lba}"
	);
}

fn memory_db_read_header(db_lba: u64) -> Result<MemoryDbHeader, ()> {
	let mut header = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	read_sectors(db_lba, 1, header.as_mut_ptr())?;

	if &header[0..MEMORY_DB_MAGIC.len()] != MEMORY_DB_MAGIC {
		return Err(());
	}

	if le_u32(header.as_slice(), MEMORY_DB_VERSION_OFFSET) != MEMORY_DB_VERSION {
		return Err(());
	}
	if le_u32(header.as_slice(), MEMORY_DB_SECTOR_SIZE_OFFSET) != SECTOR_SIZE as u32 {
		return Err(());
	}

	let mut magic = [0u8; 8];
	magic.copy_from_slice(&header[0..MEMORY_DB_MAGIC.len()]);
	let state = MemoryDbHeader {
		magic,
		version: le_u32(header.as_slice(), MEMORY_DB_VERSION_OFFSET),
		sector_size: le_u32(header.as_slice(), MEMORY_DB_SECTOR_SIZE_OFFSET),
		header_lba: le_u64(header.as_slice(), MEMORY_DB_HEADER_LBA_OFFSET),
		data_lba: le_u64(header.as_slice(), MEMORY_DB_DATA_LBA_OFFSET),
		total_sectors: le_u64(header.as_slice(), MEMORY_DB_TOTAL_SECTORS_OFFSET),
		next_append_lba: le_u64(header.as_slice(), MEMORY_DB_NEXT_APPEND_LBA_OFFSET),
		last_ssd_lba: le_u64(header.as_slice(), MEMORY_DB_LAST_SSD_LBA_OFFSET),
		model_start_lba: le_u64(header.as_slice(), MEMORY_DB_MODEL_START_LBA_OFFSET),
		model_last_lba: le_u64(header.as_slice(), MEMORY_DB_MODEL_LAST_LBA_OFFSET),
		map_lba: le_u64(header.as_slice(), MEMORY_DB_MAP_LBA_OFFSET),
		map_entries_used: le_u64(header.as_slice(), MEMORY_DB_MAP_ENTRIES_USED_OFFSET),
		map_sectors: le_u64(header.as_slice(), MEMORY_DB_MAP_SECTORS_OFFSET),
	};

	Ok(state)
}

fn memory_db_recover_from_map(state: &mut MemoryDbHeader) -> Result<(), ()> {
	let map_capacity = memory_db_map_entry_capacity();
	if state.map_lba == 0 || state.map_sectors != MEMORY_DB_MAP_SECTORS {
		warn!(
			"AHCI memory DB: invalid map region for header LBA {}: map_lba={}, map_sectors={}",
			state.header_lba, state.map_lba, state.map_sectors
		);
		return Err(());
	}

	let db_data_end = state.data_lba.saturating_add(state.total_sectors);
	let (_, used_from_map_header) = memory_db_read_map_header(state.map_lba)?;
	let mapped_entries = used_from_map_header.min(map_capacity);
	let mut cursor = state.data_lba;
	let mut recovered = 0u64;

	while recovered < mapped_entries {
		let entry = memory_db_read_map_entry(state.map_lba, recovered)?;
		if entry.flags & MEMORY_DB_MAP_FLAG_ACTIVE == 0 {
			break;
		}
		if entry.payload_bytes == 0 {
			break;
		}
		let payload_bytes = usize::try_from(entry.payload_bytes).map_err(|_| ())?;
		let entry_sectors = sectors_for_bytes(payload_bytes);
		let Some(next_cursor) = cursor.checked_add(entry_sectors) else {
			warn!("AHCI memory DB: map entry {} cursor overflow", recovered);
			return Err(());
		};
		if entry.start_lba != cursor || next_cursor > db_data_end {
			warn!(
				"AHCI memory DB: map entry {} invalid (expected start {}, found start {}, payload {} bytes, next {} > data_end {})",
				recovered, cursor, entry.start_lba, payload_bytes, next_cursor, db_data_end
			);
			break;
		}
		cursor = next_cursor;
		recovered += 1;
	}

	state.next_append_lba = cursor;
	state.map_entries_used = recovered;
	if mapped_entries != recovered {
		warn!(
			"AHCI memory DB: recovered map entries {} from header {mapped_entries}",
			recovered,
			mapped_entries = mapped_entries
		);
	}
	if state.next_append_lba < state.data_lba || state.next_append_lba > db_data_end {
		return Err(());
	}
	Ok(())
}

fn memory_db_write_header(state: &MemoryDbHeader) -> Result<(), ()> {
	let Some(&db_lba) = MEMORY_DB_HEADER_LBA.get() else {
		warn!("AHCI memory DB: header LBA not known");
		return Err(());
	};

	if state.header_lba != db_lba {
		warn!(
			"AHCI memory DB: stale header mismatch state={}, runtime={}",
			state.header_lba, db_lba
		);
		return Err(());
	}

	let mut header = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	header[0..MEMORY_DB_MAGIC.len()].copy_from_slice(&state.magic);
	write_le_u32(
		header.as_mut_slice(),
		MEMORY_DB_VERSION_OFFSET,
		state.version,
	);
	write_le_u32(
		header.as_mut_slice(),
		MEMORY_DB_SECTOR_SIZE_OFFSET,
		state.sector_size,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_HEADER_LBA_OFFSET,
		state.header_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_DATA_LBA_OFFSET,
		state.data_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_TOTAL_SECTORS_OFFSET,
		state.total_sectors,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_NEXT_APPEND_LBA_OFFSET,
		state.next_append_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_LAST_SSD_LBA_OFFSET,
		state.last_ssd_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_MODEL_START_LBA_OFFSET,
		state.model_start_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_MODEL_LAST_LBA_OFFSET,
		state.model_last_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_MAP_LBA_OFFSET,
		state.map_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_MAP_ENTRIES_USED_OFFSET,
		state.map_entries_used,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_MAP_SECTORS_OFFSET,
		state.map_sectors,
	);

	write_sectors(db_lba, 1, header.as_ptr())
}

pub fn append_memory_db(data_ptr: *const u8, data_len: usize) -> Result<usize, ()> {
	let Some(&db_lba) = MEMORY_DB_HEADER_LBA.get() else {
		warn!("AHCI memory DB: append called before DB was registered");
		return Err(());
	};
	if data_ptr.is_null() || data_len == 0 {
		return Ok(0);
	}

	let mut state = memory_db_read_header(db_lba)?;
	let expected_map_lba = db_lba.saturating_add(1);
	if state.map_lba != expected_map_lba || state.map_sectors != MEMORY_DB_MAP_SECTORS {
		warn!(
			"AHCI memory DB: map layout mismatch header LBA {}; expected map LBA {}, got {}, expected map sectors {}, got {}",
			db_lba, expected_map_lba, state.map_lba, MEMORY_DB_MAP_SECTORS, state.map_sectors
		);
		return Err(());
	}
	if let Err(_) = memory_db_recover_from_map(&mut state) {
		warn!(
			"AHCI memory DB: map recovery failed for header LBA {}; using header cursor {}",
			state.header_lba, state.next_append_lba
		);
	}
	let map_capacity = memory_db_map_entry_capacity();
	if state.map_entries_used >= map_capacity {
		warn!(
			"AHCI memory DB: map full at {} entries (limit {})",
			state.map_entries_used, map_capacity
		);
		return Err(());
	}
	let Some(&capacity_sectors) = SSD_SECTOR_COUNT.get() else {
		warn!("AHCI memory DB: SSD sector count unknown");
		return Err(());
	};

	let db_data_end = state.data_lba.saturating_add(state.total_sectors);
	let disk_last_exclusive = capacity_sectors.saturating_add(1);
	let db_end_exclusive = core::cmp::min(db_data_end, disk_last_exclusive);
	if state.next_append_lba < state.data_lba || state.next_append_lba >= db_end_exclusive {
		warn!(
			"AHCI memory DB: next append LBA {} outside usable range {}..{}",
			state.next_append_lba,
			state.data_lba,
			db_end_exclusive.saturating_sub(1)
		);
		return Err(());
	}

	let mut remaining = data_len;
	let mut written = 0usize;
	let mut write_lba = state.next_append_lba;
	while remaining > 0 {
		let available_lba = db_end_exclusive.saturating_sub(write_lba);
		if available_lba == 0 {
			warn!("AHCI memory DB: full at LBA {}", write_lba);
			return Err(());
		}

		let available_bytes = match usize::try_from(available_lba) {
			Ok(bytes) => bytes.saturating_mul(SECTOR_SIZE),
			Err(_) => usize::MAX,
		};
		let mut chunk_len = remaining.min(available_bytes);
		if chunk_len > MODEL_IO_SECTORS as usize * SECTOR_SIZE {
			chunk_len = MODEL_IO_SECTORS as usize * SECTOR_SIZE;
		}
		if chunk_len == 0 {
			return Err(());
		}

		let full_sectors = chunk_len / SECTOR_SIZE;
		let tail_bytes = chunk_len - (full_sectors * SECTOR_SIZE);
		let mut sectors_written = 0u64;

		if full_sectors > 0 {
			let sectors = u16::try_from(full_sectors).map_err(|_| ())?;
			let src = unsafe { data_ptr.add(written) };
			write_sectors(write_lba, sectors, src)?;
			write_lba += u64::from(sectors);
			sectors_written += u64::from(sectors);
			written += full_sectors * SECTOR_SIZE;
			remaining -= full_sectors * SECTOR_SIZE;
		}

		if tail_bytes > 0 {
			let mut tail = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
			let src = unsafe { core::slice::from_raw_parts(data_ptr.add(written), tail_bytes) };
			tail[..tail_bytes].copy_from_slice(src);
			write_sectors(write_lba, 1, tail.as_ptr())?;
			write_lba += 1;
			sectors_written += 1;
			written += tail_bytes;
			remaining -= tail_bytes;
		}

		if sectors_written == 0 {
			return Err(());
		}
	}

	let payload_u32 = u32::try_from(data_len).map_err(|_| ())?;
	memory_db_write_map_entry(
		state.map_lba,
		state.map_entries_used,
		state.next_append_lba,
		payload_u32,
		MEMORY_DB_MAP_FLAG_ACTIVE,
	)?;
	state.map_entries_used += 1;
	state.next_append_lba = write_lba;
	memory_db_write_map_header(state.map_lba, state.map_entries_used)?;
	memory_db_write_header(&state)?;
	Ok(written)
}

pub fn erase_memory_db() -> Result<(), ()> {
	let Some(&db_lba) = MEMORY_DB_HEADER_LBA.get() else {
		warn!("AHCI memory DB: erase called before DB was registered");
		return Err(());
	};

	let state = memory_db_read_header(db_lba)?;
	let expected_map_lba = db_lba.saturating_add(1);
	if state.header_lba != db_lba
		|| state.map_lba != expected_map_lba
		|| state.map_sectors != MEMORY_DB_MAP_SECTORS
		|| state.total_sectors != MEMORY_DB_TOTAL_SECTORS
	{
		warn!(
			"AHCI memory DB: refusing erase due to layout mismatch header={}, map={}, map_sectors={}, total={}",
			state.header_lba, state.map_lba, state.map_sectors, state.total_sectors
		);
		return Err(());
	}

	let Some(&capacity_sectors) = SSD_SECTOR_COUNT.get() else {
		warn!("AHCI memory DB: SSD sector count unknown during erase");
		return Err(());
	};
	let db_end_exclusive = db_lba.checked_add(MEMORY_DB_TOTAL_SECTORS).ok_or(())?;
	if db_end_exclusive > capacity_sectors {
		warn!(
			"AHCI memory DB: refusing erase range {}..{} beyond SSD last LBA {}",
			db_lba,
			db_end_exclusive.saturating_sub(1),
			capacity_sectors.saturating_sub(1)
		);
		return Err(());
	}

	let zero = Box::new_in([0u8; MEMORY_DB_ERASE_CHUNK_BYTES], DeviceAlloc);
	let mut lba = db_lba;
	while lba < db_end_exclusive {
		let remaining = db_end_exclusive.saturating_sub(lba);
		let sectors = u16::try_from(remaining.min(u64::from(MEMORY_DB_ERASE_CHUNK_SECTORS)))
			.map_err(|_| ())?;
		write_sectors(lba, sectors, zero.as_ptr())?;
		lba = lba.saturating_add(u64::from(sectors));
	}

	memory_db_write_map_header(state.map_lba, 0)?;
	let fresh = MemoryDbHeader {
		magic: *MEMORY_DB_MAGIC,
		version: MEMORY_DB_VERSION,
		sector_size: SECTOR_SIZE as u32,
		header_lba: db_lba,
		data_lba: state.data_lba,
		total_sectors: MEMORY_DB_TOTAL_SECTORS,
		next_append_lba: state.data_lba,
		last_ssd_lba: capacity_sectors.saturating_sub(1),
		model_start_lba: state.model_start_lba,
		model_last_lba: state.model_last_lba,
		map_lba: state.map_lba,
		map_entries_used: 0,
		map_sectors: MEMORY_DB_MAP_SECTORS,
	};
	memory_db_write_header(&fresh)?;
	Ok(())
}

pub fn read_memory_db(buf: *mut u8, buf_len: usize) -> Result<usize, ()> {
	let Some(&db_lba) = MEMORY_DB_HEADER_LBA.get() else {
		warn!("AHCI memory DB: read called before DB was registered");
		return Err(());
	};
	if buf.is_null() || buf_len == 0 {
		return Ok(0);
	}

	let mut state = memory_db_read_header(db_lba)?;
	let expected_map_lba = db_lba.saturating_add(1);
	if state.map_lba != expected_map_lba || state.map_sectors != MEMORY_DB_MAP_SECTORS {
		warn!(
			"AHCI memory DB: map layout mismatch header LBA {}; expected map LBA {}, got {}, expected map sectors {}, got {}",
			db_lba, expected_map_lba, state.map_lba, MEMORY_DB_MAP_SECTORS, state.map_sectors
		);
		return Err(());
	}

	if let Err(_) = memory_db_recover_from_map(&mut state) {
		warn!(
			"AHCI memory DB: map recovery failed for header LBA {}; using header cursor {}",
			state.header_lba, state.next_append_lba
		);
	}

	let Some(&capacity_sectors) = SSD_SECTOR_COUNT.get() else {
		warn!("AHCI memory DB: SSD sector count unknown");
		return Err(());
	};
	let db_data_end = state.data_lba.saturating_add(state.total_sectors);
	let usable_tail = if capacity_sectors > db_data_end {
		capacity_sectors
	} else {
		db_data_end
	};
	if state.next_append_lba < state.data_lba || state.next_append_lba > db_data_end {
		warn!(
			"AHCI memory DB: cursor out of data range {}..{}",
			state.data_lba,
			db_data_end.saturating_sub(1)
		);
		return Err(());
	}

	let mut entries_remaining = state.map_entries_used.min(memory_db_map_entry_capacity());
	if entries_remaining == 0 {
		return Ok(0);
	}
	let mut read_ptr = buf;
	let mut remaining = buf_len;
	let mut total_read = 0usize;
	let mut sector = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);

	while entries_remaining > 0 && remaining > 0 {
		let entry_index = state.map_entries_used - entries_remaining;
		let entry = memory_db_read_map_entry(state.map_lba, entry_index)?;
		if entry.flags & MEMORY_DB_MAP_FLAG_ACTIVE == 0 {
			break;
		}
		if entry.payload_bytes == 0 {
			break;
		}

		let payload_bytes = usize::try_from(entry.payload_bytes).map_err(|_| ())?;
		if payload_bytes == 0 {
			break;
		}

		let mut entry_offset = 0usize;
		while entry_offset < payload_bytes && remaining > 0 {
			let bytes_left = payload_bytes - entry_offset;
			let to_copy = bytes_left.min(remaining).min(SECTOR_SIZE);
			let sector_index = entry_offset / SECTOR_SIZE;
			let read_lba = match entry.start_lba.checked_add(sector_index as u64) {
				Some(v) => v,
				None => return Err(()),
			};
			if read_lba >= usable_tail {
				error!(
					"AHCI memory DB: read LBA {} beyond DB capacity {}",
					read_lba, usable_tail
				);
				return Err(());
			}

			if read_sectors(read_lba, 1, sector.as_mut_ptr()).is_err() {
				warn!(
					"AHCI memory DB: failed reading log sector {}@{}",
					entry_index, read_lba
				);
				return Err(());
			}

			// Safety: read_ptr and to_copy are bounded to buffer capacity by remaining.
			unsafe {
				core::ptr::copy_nonoverlapping(sector.as_ptr(), read_ptr, to_copy);
			}

			total_read += to_copy;
			unsafe {
				read_ptr = read_ptr.add(to_copy);
			}
			remaining = remaining.saturating_sub(to_copy);
			entry_offset = entry_offset.saturating_add(to_copy);
		}

		entries_remaining = entries_remaining.saturating_sub(1);
	}

	Ok(total_read)
}

fn ensure_memory_db(db_lba: u64, model_start_lba: u64, model_last_lba: u64) -> Result<(), ()> {
	let Some(&capacity_sectors) = SSD_SECTOR_COUNT.get() else {
		warn!("AHCI memory DB: SSD capacity unknown; skipping DB initialization");
		return Err(());
	};
	let Some(db_end_exclusive) = db_lba.checked_add(MEMORY_DB_TOTAL_SECTORS) else {
		error!("AHCI memory DB: DB range overflow at LBA {}", db_lba);
		return Err(());
	};
	if db_end_exclusive > capacity_sectors {
		error!(
			"AHCI memory DB: DB range LBA {}..{} exceeds SSD last LBA {}",
			db_lba,
			db_end_exclusive.saturating_sub(1),
			capacity_sectors.saturating_sub(1)
		);
		return Err(());
	}

	let db_map_lba = db_lba.saturating_add(1);
	let db_data_lba = db_map_lba.saturating_add(MEMORY_DB_MAP_SECTORS);
	if db_data_lba >= db_end_exclusive {
		error!(
			"AHCI memory DB: DB map consumes header range at LBA {}..{}; no space left for data",
			db_map_lba,
			db_end_exclusive.saturating_sub(1)
		);
		return Err(());
	}
	let db_last_lba = db_end_exclusive - 1;
	info!(
		"AHCI memory DB: raw DB range starts at LBA {}, data LBA {}..{} ({} sectors total)",
		db_lba, db_data_lba, db_last_lba, MEMORY_DB_TOTAL_SECTORS
	);
	info!(
		"AHCI memory DB: model protected range LBA {}..{}; DB starts immediately after model",
		model_start_lba, model_last_lba
	);

	let mut existing = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	if read_sectors(db_lba, 1, existing.as_mut_ptr()).is_ok() {
		let existing_magic_ok = &existing[0..MEMORY_DB_MAGIC.len()] == MEMORY_DB_MAGIC;
		let existing_version = le_u32(existing.as_slice(), MEMORY_DB_VERSION_OFFSET);
		let existing_sector_size = le_u32(existing.as_slice(), MEMORY_DB_SECTOR_SIZE_OFFSET);
		let existing_header_lba = le_u64(existing.as_slice(), MEMORY_DB_HEADER_LBA_OFFSET);
		let existing_data_lba = le_u64(existing.as_slice(), MEMORY_DB_DATA_LBA_OFFSET);
		let existing_total_sectors = le_u64(existing.as_slice(), MEMORY_DB_TOTAL_SECTORS_OFFSET);
		let existing_next_append_lba =
			le_u64(existing.as_slice(), MEMORY_DB_NEXT_APPEND_LBA_OFFSET);
		let existing_map_lba = le_u64(existing.as_slice(), MEMORY_DB_MAP_LBA_OFFSET);
		let existing_map_entries_used =
			le_u64(existing.as_slice(), MEMORY_DB_MAP_ENTRIES_USED_OFFSET);
		let existing_map_sectors = le_u64(existing.as_slice(), MEMORY_DB_MAP_SECTORS_OFFSET);
		let map_read_ok = memory_db_read_map_header(existing_map_lba).is_ok();

		if existing_magic_ok
			&& existing_version == MEMORY_DB_VERSION
			&& existing_sector_size == SECTOR_SIZE as u32
			&& existing_header_lba == db_lba
			&& existing_data_lba == db_data_lba
			&& existing_map_lba == db_map_lba
			&& existing_map_sectors == MEMORY_DB_MAP_SECTORS
			&& existing_total_sectors == MEMORY_DB_TOTAL_SECTORS
			&& existing_next_append_lba >= db_data_lba
			&& existing_next_append_lba <= db_end_exclusive
			&& existing_map_entries_used <= memory_db_map_entry_capacity()
			&& map_read_ok
		{
			let mut state = MemoryDbHeader {
				magic: *MEMORY_DB_MAGIC,
				version: existing_version,
				sector_size: existing_sector_size,
				header_lba: existing_header_lba,
				data_lba: existing_data_lba,
				total_sectors: existing_total_sectors,
				next_append_lba: existing_next_append_lba,
				last_ssd_lba: le_u64(existing.as_slice(), MEMORY_DB_LAST_SSD_LBA_OFFSET),
				model_start_lba,
				model_last_lba,
				map_lba: existing_map_lba,
				map_entries_used: existing_map_entries_used,
				map_sectors: existing_map_sectors,
			};

			if memory_db_recover_from_map(&mut state).is_ok() {
				memory_db_write_map_header(state.map_lba, state.map_entries_used)?;
				memory_db_register_state(db_lba, db_data_lba);
				memory_db_write_header(&state)?;
				info!(
					"AHCI memory DB: existing header valid at LBA {}, preserving DB; next append LBA {}",
					db_lba, state.next_append_lba
				);
				return Ok(());
			}
		}

		info!(
			"AHCI memory DB: no valid header at LBA {}; first16={:02x?}; writing fresh header",
			db_lba,
			&existing[0..16]
		);
	} else {
		warn!(
			"AHCI memory DB: failed to read DB header LBA {}; writing fresh header",
			db_lba
		);
	}

	let mut header = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	header[0..MEMORY_DB_MAGIC.len()].copy_from_slice(MEMORY_DB_MAGIC);
	write_le_u32(
		header.as_mut_slice(),
		MEMORY_DB_VERSION_OFFSET,
		MEMORY_DB_VERSION,
	);
	write_le_u32(
		header.as_mut_slice(),
		MEMORY_DB_SECTOR_SIZE_OFFSET,
		SECTOR_SIZE as u32,
	);
	write_le_u64(header.as_mut_slice(), MEMORY_DB_HEADER_LBA_OFFSET, db_lba);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_DATA_LBA_OFFSET,
		db_data_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_TOTAL_SECTORS_OFFSET,
		MEMORY_DB_TOTAL_SECTORS,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_NEXT_APPEND_LBA_OFFSET,
		db_data_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_LAST_SSD_LBA_OFFSET,
		capacity_sectors.saturating_sub(1),
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_MODEL_START_LBA_OFFSET,
		model_start_lba,
	);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_MODEL_LAST_LBA_OFFSET,
		model_last_lba,
	);
	write_le_u64(header.as_mut_slice(), MEMORY_DB_MAP_LBA_OFFSET, db_map_lba);
	write_le_u64(header.as_mut_slice(), MEMORY_DB_MAP_ENTRIES_USED_OFFSET, 0);
	write_le_u64(
		header.as_mut_slice(),
		MEMORY_DB_MAP_SECTORS_OFFSET,
		MEMORY_DB_MAP_SECTORS,
	);

	let zero = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	for i in 0..MEMORY_DB_MAP_SECTORS {
		write_sectors(db_map_lba + i, 1, zero.as_ptr())?;
	}
	memory_db_write_map_header(db_map_lba, 0)?;

	write_sectors(db_lba, 1, header.as_ptr())?;

	let mut verify = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	read_sectors(db_lba, 1, verify.as_mut_ptr())?;
	if &verify[0..MEMORY_DB_MAGIC.len()] != MEMORY_DB_MAGIC {
		error!(
			"AHCI memory DB: header verify failed at LBA {}, got {:02x?}",
			db_lba,
			&verify[0..16]
		);
		return Err(());
	}
	info!(
		"AHCI memory DB: fresh header written and verified at LBA {}, first16={:02x?}",
		db_lba,
		&verify[0..16]
	);
	memory_db_register_state(db_lba, db_data_lba);
	Ok(())
}

fn verify_raw_model_extent(data_lba: u64, model_size: usize) -> bool {
	let sector_count = sectors_for_bytes(model_size);
	if sector_count == 0 {
		error!("AHCI raw model cache: cannot verify zero-sector model extent");
		return false;
	}
	let last_lba = data_lba + sector_count - 1;
	let next_free_lba = data_lba + sector_count;
	let valid_tail_bytes = match model_size % SECTOR_SIZE {
		0 => SECTOR_SIZE,
		remainder => remainder,
	};
	let padding_bytes = SECTOR_SIZE - valid_tail_bytes;

	let mut last_sector = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	if read_sectors(last_lba, 1, last_sector.as_mut_ptr()).is_err() {
		error!(
			"AHCI raw model cache: failed to read computed final model LBA {}",
			last_lba
		);
		return false;
	}
	info!(
		"AHCI raw model cache: verified final model LBA {} is readable; valid bytes in final sector {}, padding bytes {}",
		last_lba, valid_tail_bytes, padding_bytes
	);
	info!(
		"AHCI raw model cache: final model sector first16={:02x?} final_valid_tail={:02x?}",
		&last_sector[0..16],
		&last_sector[valid_tail_bytes.saturating_sub(16)..valid_tail_bytes]
	);

	if let Some(&capacity_sectors) = SSD_SECTOR_COUNT.get()
		&& next_free_lba < capacity_sectors
	{
		let mut next_sector = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
		if read_sectors(next_free_lba, 1, next_sector.as_mut_ptr()).is_ok() {
			info!(
				"AHCI raw model cache: read-only next-free LBA {} probe first16={:02x?}",
				next_free_lba,
				&next_sector[0..16]
			);
		} else {
			warn!(
				"AHCI raw model cache: read-only next-free LBA {} probe failed",
				next_free_lba
			);
		}
	}

	if ensure_memory_db(next_free_lba, data_lba, last_lba).is_err() {
		warn!("AHCI memory DB: initialization failed; continuing with read-only model cache path");
	}

	true
}

pub fn write_raw_model_cache(model_ptr: *const u8, model_size: usize) -> Result<(), ()> {
	let Some(cache_lba) = discover_model_cache_lba() else {
		warn!("AHCI raw model cache: no usable fixed cache LBA");
		return Err(());
	};

	let data_lba = cache_lba + MODEL_HEADER_SECTORS;
	log_raw_model_lba_range("write", data_lba, model_size);
	if !raw_model_range_fits(data_lba, model_size) {
		return Err(());
	}
	info!(
		"AHCI raw model cache: writing {} bytes at fixed cache header LBA {}, data LBA {}",
		model_size, cache_lba, data_lba
	);
	if smoke_test_raw_model_cache(cache_lba).is_err() {
		error!("AHCI raw model cache: write smoke test failed; skipping model cache write");
		return Err(());
	}

	let mut header = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	write_model_header(
		header.as_mut_slice(),
		MODEL_MAGIC_WRITING,
		model_size as u64,
		data_lba,
	);
	write_sectors(cache_lba, 1, header.as_ptr())?;

	let mut offset = 0usize;
	while offset < model_size {
		let remaining = model_size - offset;
		let chunk_bytes = remaining.min(MODEL_IO_SECTORS as usize * SECTOR_SIZE);
		let sectors = chunk_bytes.div_ceil(SECTOR_SIZE) as u16;
		let src = unsafe { model_ptr.add(offset) };
		write_sectors(data_lba + (offset / SECTOR_SIZE) as u64, sectors, src)?;
		offset += sectors as usize * SECTOR_SIZE;
		core_scheduler().reschedule();
	}

	write_model_header(
		header.as_mut_slice(),
		MODEL_MAGIC_READY,
		model_size as u64,
		data_lba,
	);
	write_sectors(cache_lba, 1, header.as_ptr())?;
	let sector_count = sectors_for_bytes(model_size);
	let last_lba = data_lba + sector_count.saturating_sub(1);
	let next_free_lba = data_lba + sector_count;
	if ensure_memory_db(next_free_lba, data_lba, last_lba).is_err() {
		warn!("AHCI memory DB: initialization failed after model cache write");
	}
	info!("AHCI raw model cache: write complete");
	Ok(())
}

fn smoke_test_raw_model_cache(cache_lba: u64) -> Result<(), ()> {
	let test_lba = cache_lba + 1;
	let mut write_buf = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	let mut read_buf = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	write_buf[0..8].copy_from_slice(b"HSMOKE01");
	write_buf[8..16].copy_from_slice(&test_lba.to_le_bytes());

	info!(
		"AHCI raw model cache: smoke testing fixed cache writable LBA {}",
		test_lba
	);
	write_sectors(test_lba, 1, write_buf.as_ptr())?;
	read_sectors(test_lba, 1, read_buf.as_mut_ptr())?;
	if read_buf[0..16] != write_buf[0..16] {
		error!(
			"AHCI raw model cache: smoke test verify mismatch, got {:02x?}",
			&read_buf[0..16]
		);
		return Err(());
	}
	info!("AHCI raw model cache: fixed cache smoke test passed");
	Ok(())
}

pub fn try_load_raw_model() -> bool {
	if env::model_info().is_some() {
		info!("AHCI raw model cache: model already loaded, skipping SSD cache read");
		return true;
	}

	info!("AHCI raw model cache: using fixed SSD cache LBA");
	let Some(cache_lba) = discover_model_cache_lba() else {
		warn!("AHCI raw model cache: no usable fixed cache LBA");
		return false;
	};

	info!(
		"AHCI raw model cache: trying SSD cache header at fixed LBA {}",
		cache_lba
	);
	let mut header = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	if read_sectors(cache_lba, 1, header.as_mut_ptr()).is_err() {
		warn!("AHCI raw model cache: failed to read header");
		return false;
	}
	if &header[0..8] != MODEL_MAGIC_READY {
		warn!(
			"AHCI raw model cache: header not ready, magic {:02x?}",
			&header[0..8]
		);
		return false;
	}
	if &header[MODEL_CACHE_TAG_OFFSET..MODEL_CACHE_TAG_OFFSET + MODEL_CACHE_TAG.len()]
		!= MODEL_CACHE_TAG
	{
		warn!(
			"AHCI raw model cache: header tag is not BITNET2B, got {:02x?}; ignoring SSD cache so USB can recache",
			&header[MODEL_CACHE_TAG_OFFSET..MODEL_CACHE_TAG_OFFSET + MODEL_CACHE_TAG.len()]
		);
		return false;
	}

	let model_size = le_u64(header.as_slice(), 8) as usize;
	let data_lba = le_u64(header.as_slice(), 16);
	if data_lba != cache_lba + MODEL_HEADER_SECTORS {
		warn!(
			"AHCI raw model cache: header data LBA {} does not match fixed data LBA {}; ignoring SSD cache",
			data_lba,
			cache_lba + MODEL_HEADER_SECTORS
		);
		return false;
	}
	if model_size == 0 {
		warn!("AHCI raw model cache: header has zero model size");
		return false;
	}
	if !(MIN_RAW_MODEL_SIZE..=MAX_RAW_MODEL_SIZE).contains(&model_size) {
		warn!(
			"AHCI raw model cache: cache size {} outside allowed range {}..={}; ignoring SSD cache so USB can recache",
			model_size, MIN_RAW_MODEL_SIZE, MAX_RAW_MODEL_SIZE
		);
		return false;
	}
	log_raw_model_lba_range("read", data_lba, model_size);
	if !raw_model_range_fits(data_lba, model_size) {
		return false;
	}
	if !verify_raw_model_extent(data_lba, model_size) {
		return false;
	}

	let model_load_start_us = crate::arch::processor::get_timer_ticks();
	let alloc_size =
		align_address::Align::align_up(model_size, MODEL_IO_SECTORS as usize * SECTOR_SIZE);
	let layout = Layout::from_size_align(alloc_size, 4096).unwrap();
	let target_base = unsafe {
		let ptr = alloc::alloc::alloc(layout);
		if ptr.is_null() {
			error!(
				"AHCI raw model cache: failed to allocate {} bytes",
				alloc_size
			);
			return false;
		}
		ptr.expose_provenance()
	};

	info!(
		"AHCI raw model cache: loading {} bytes from LBA {} into {:#x}",
		model_size, data_lba, target_base
	);
	let mut offset = 0usize;
	let mut next_progress = MODEL_LOAD_PROGRESS_INTERVAL;
	while offset < model_size {
		let remaining = model_size - offset;
		let chunk_bytes = remaining.min(MODEL_IO_SECTORS as usize * SECTOR_SIZE);
		let sectors = chunk_bytes.div_ceil(SECTOR_SIZE) as u16;
		let dst = core::ptr::with_exposed_provenance_mut::<u8>(target_base + offset);
		if read_sectors(data_lba + (offset / SECTOR_SIZE) as u64, sectors, dst).is_err() {
			error!(
				"AHCI raw model cache: read failed at offset {} LBA {}",
				offset,
				data_lba + (offset / SECTOR_SIZE) as u64
			);
			return false;
		}
		if offset == 0 {
			let first_sector = unsafe { slice::from_raw_parts(dst, SECTOR_SIZE) };
			info!(
				"AHCI raw model cache: first model bytes at LBA {}: {:02x?}",
				data_lba,
				&first_sector[0..16]
			);
			if &first_sector[0..4] != b"GGUF" {
				error!("AHCI raw model cache: cached model does not start with GGUF magic");
				return false;
			}
			info!(
				"AHCI raw model cache: GGUF magic verified at LBA {}",
				data_lba
			);
		}
		offset += sectors as usize * SECTOR_SIZE;
		if offset >= next_progress || offset >= model_size {
			info!(
				"AHCI raw model cache: load progress {}/{} MiB",
				offset.min(model_size) / (1024 * 1024),
				model_size / (1024 * 1024)
			);
			while next_progress <= offset {
				next_progress += MODEL_LOAD_PROGRESS_INTERVAL;
			}
		}
		core_scheduler().reschedule();
	}

	env::set_model_info_with_source(target_base, model_size, "ssd-cache");
	let model_load_us =
		crate::arch::processor::get_timer_ticks().saturating_sub(model_load_start_us);
	let mib_per_s = if model_load_us > 0 {
		(model_size as u64).saturating_mul(1_000_000) / model_load_us / (1024 * 1024)
	} else {
		0
	};
	info!(
		"PERF model_load source=ssd-cache bytes={} elapsed_us={} throughput_mib_s={} address={:#x}",
		model_size, model_load_us, mib_per_s, target_base
	);
	info!("AHCI raw model cache: model loaded into RAM from SSD cache");
	true
}

fn discover_model_cache_lba() -> Option<u64> {
	if let Some(&lba) = MODEL_CACHE_START_LBA.get() {
		return Some(lba);
	}

	let _ = MODEL_CACHE_START_LBA.set(MODEL_FIXED_CACHE_LBA);
	info!(
		"AHCI raw model cache: using fixed header LBA {}, data LBA {}",
		MODEL_FIXED_CACHE_LBA,
		MODEL_FIXED_CACHE_LBA + MODEL_HEADER_SECTORS
	);
	Some(MODEL_FIXED_CACHE_LBA)
}

#[allow(dead_code)]
fn discover_model_partition() -> Option<u64> {
	let mut header = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	info!("AHCI GPT: reading primary GPT header at LBA 1");
	if read_sectors(1, 1, header.as_mut_ptr()).is_err() {
		error!("AHCI GPT: failed to read primary GPT header at LBA 1");
		return None;
	}
	if &header[0..8] != b"EFI PART" {
		warn!(
			"AHCI GPT: missing EFI PART signature at LBA 1, first 16 bytes {:02x?}",
			&header[0..16]
		);
		return None;
	}

	let entries_lba = le_u64(header.as_slice(), 72);
	let entry_count = le_u32(header.as_slice(), 80) as usize;
	let entry_size = le_u32(header.as_slice(), 84) as usize;
	info!(
		"AHCI GPT: header OK, entries_lba {}, entry_count {}, entry_size {}",
		entries_lba, entry_count, entry_size
	);
	if entry_size == 0 || entry_size > SECTOR_SIZE {
		warn!("AHCI GPT: unsupported entry size {}", entry_size);
		return None;
	}

	let mut sector = Box::new_in([0u8; SECTOR_SIZE], DeviceAlloc);
	for index in 0..entry_count {
		let byte_offset = index * entry_size;
		let lba = entries_lba + (byte_offset / SECTOR_SIZE) as u64;
		let offset = byte_offset % SECTOR_SIZE;
		if offset + entry_size > SECTOR_SIZE {
			continue;
		}
		if read_sectors(lba, 1, sector.as_mut_ptr()).is_err() {
			error!(
				"AHCI GPT: failed to read partition entry sector at LBA {} for index {}",
				lba,
				index + 1
			);
			return None;
		}
		let entry = &sector[offset..offset + entry_size];
		if entry[0..16].iter().all(|&b| b == 0) {
			continue;
		}
		let first_lba = le_u64(entry, 32);
		let last_lba = le_u64(entry, 40);
		let name = gpt_partition_name(&entry[56..entry_size]);
		info!(
			"AHCI GPT: partition index {}, LBA {}..{}, name '{}'",
			index + 1,
			first_lba,
			last_lba,
			name
		);

		info!(
			"AHCI GPT: using raw model cache partition index {}, LBA {}..{}, name '{}'",
			index + 1,
			first_lba,
			last_lba,
			name
		);
		return Some(first_lba);
	}

	warn!("AHCI GPT: no non-empty partitions found");
	None
}

fn write_model_header(header: &mut [u8], magic: &[u8; 8], model_size: u64, data_lba: u64) {
	header.fill(0);
	header[0..8].copy_from_slice(magic);
	header[8..16].copy_from_slice(&model_size.to_le_bytes());
	header[16..24].copy_from_slice(&data_lba.to_le_bytes());
	header[MODEL_CACHE_TAG_OFFSET..MODEL_CACHE_TAG_OFFSET + MODEL_CACHE_TAG.len()]
		.copy_from_slice(MODEL_CACHE_TAG);
}

fn gpt_partition_name(raw_name: &[u8]) -> String {
	let mut name = String::new();
	for code_unit in raw_name.chunks_exact(2) {
		let ch = u16::from_le_bytes([code_unit[0], code_unit[1]]);
		if ch == 0 {
			break;
		}
		if let Some(ch) = char::from_u32(u32::from(ch)) {
			name.push(ch);
		} else {
			name.push('?');
		}
	}
	name
}

fn le_u32(buf: &[u8], offset: usize) -> u32 {
	u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

fn le_u64(buf: &[u8], offset: usize) -> u64 {
	u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}

fn transfer_sectors(
	port: *mut u32,
	lba: u64,
	count: u16,
	buf_virt: *mut u8,
	is_write: bool,
) -> Result<(), ()> {
	trace!(
		"AHCI: transfer_sectors lba={}, count={}, is_write={}",
		lba, count, is_write
	);
	// Allocate necessary Command Table (this one can be local as we wait for completion)
	let mut ct = Box::new_in(
		CommandTable {
			cfis: [0; 64],
			acmd: [0; 16],
			reserved: [0; 48],
			prdt: [PRDTEntry::default()],
		},
		DeviceAlloc,
	);

	let Some(&cl_addr) = FIRST_SATA_PORT_CL.get() else {
		return Err(());
	};
	let cl_ptr = cl_addr as *mut CommandHeader;

	let ct_phys = mm::virtual_to_physical(VirtAddr::from_ptr(ct.as_ref() as *const _ as *const u8))
		.unwrap()
		.as_u64();
	let data_phys = mm::virtual_to_physical(VirtAddr::new(buf_virt as u64))
		.unwrap()
		.as_u64();

	// Wait for port to be ready
	let mut wait = 10000;
	while (unsafe { port.add(8).read_volatile() } & ((1 << 7) | (1 << 3))) != 0 && wait > 0 {
		wait -= 1;
		crate::arch::processor::udelay(10);
	}
	if wait == 0 {
		let px_tfd = unsafe { port.add(8).read_volatile() };
		error!(
			"AHCI: port not ready before transfer lba={}, count={}, is_write={}, PxTFD={:#x}",
			lba, count, is_write, px_tfd
		);
		return Err(());
	}

	unsafe {
		port.add(4).write_volatile(0xffffffff); // PxIS
		port.add(12).write_volatile(0xffffffff); // PxSERR
	}

	let mut header = CommandHeader::default();
	header.cfl_flags = 5 | (if is_write { 1 << 6 } else { 0 });
	header.prdtl = 1;
	header.ctba = ct_phys as u32;
	header.ctbau = (ct_phys >> 32) as u32;

	let mut prdt = PRDTEntry::default();
	prdt.dba = data_phys as u32;
	prdt.dbau = (data_phys >> 32) as u32;
	prdt.dbc_flags = (u32::from(count) * 512 - 1) & 0x3f_ffff;

	ct.cfis[0] = 0x27; // Register H2D FIS
	ct.cfis[1] = 0x80; // Command bit set
	ct.cfis[2] = if is_write { 0x35 } else { 0x25 }; // WRITE DMA EXT or READ DMA EXT
	ct.cfis[4] = (lba & 0xff) as u8;
	ct.cfis[5] = ((lba >> 8) & 0xff) as u8;
	ct.cfis[6] = ((lba >> 16) & 0xff) as u8;
	ct.cfis[7] = 0x40; // LBA mode
	ct.cfis[8] = ((lba >> 24) & 0xff) as u8;
	ct.cfis[9] = ((lba >> 32) & 0xff) as u8;
	ct.cfis[10] = ((lba >> 40) & 0xff) as u8;
	ct.cfis[12] = (count & 0xff) as u8;
	ct.cfis[13] = (count >> 8) as u8;
	ct.prdt[0] = prdt;

	unsafe {
		cl_ptr.write_volatile(header);
		trace!(
			"AHCI: issuing {} command lba={}, count={}",
			if is_write { "write" } else { "read" },
			lba,
			count
		);
		port.add(14).write_volatile(1); // PxCI bit 0

		let mut timeout = if is_write { 50_000 } else { 1_000 };
		while port.add(14).read_volatile() & 1 != 0 && timeout > 0 {
			if port.add(4).read_volatile() & (1 << 30) != 0 {
				let px_is = port.add(4).read_volatile();
				let px_serr = port.add(12).read_volatile();
				let px_tfd = port.add(8).read_volatile();
				error!(
					"AHCI: transfer error! lba={lba}, count={count}, is_write={is_write}, PxIS={px_is:#x}, PxSERR={px_serr:#x}, PxTFD={px_tfd:#x}"
				);
				return Err(());
			}
			crate::arch::processor::udelay(100);
			timeout -= 1;
		}

		if timeout == 0 {
			let px_is = port.add(4).read_volatile();
			let px_tfd = port.add(8).read_volatile();
			error!(
				"AHCI: transfer timed out! lba={}, count={}, is_write={}, PxIS={:#x}, PxTFD={:#x}",
				lba, count, is_write, px_is, px_tfd
			);
			return Err(());
		}
	}

	trace!("AHCI: transfer_sectors completed successfully");
	Ok(())
}

pub fn dump_boot_log() {
	info!("AHCI: Attempting to dump boot log to disk...");
	let Some(fb) = env::framebuffer_info() else {
		warn!("AHCI: No framebuffer info available for dump");
		return;
	};

	let fb_size = fb.stride * fb.height;
	let sectors = ((fb_size + 511) / 512) as u16;

	// Map framebuffer
	let fb_virt = mm::map(
		PhysAddr::new(fb.address as u64),
		fb_size,
		false,
		true,
		false,
	);
	let fb_ptr = fb_virt.as_ptr::<u8>();

	info!(
		"AHCI: Writing {} sectors of framebuffer log at {:#x}",
		sectors, fb.address
	);

	let chunk_size = 4096;
	let mut verify_buf = Box::new_in([0u8; 4096], DeviceAlloc);

	let mut offset = 0;
	let mut success = true;
	while offset < fb_size {
		let current_chunk_size = core::cmp::min(chunk_size, fb_size - offset);
		let current_sectors = ((current_chunk_size + 511) / 512) as u16;

		if write_sectors(1024 + (offset / 512) as u64, current_sectors, unsafe {
			fb_ptr.add(offset)
		})
		.is_err()
		{
			error!("AHCI: Failed to write chunk at offset {}", offset);
			success = false;
			break;
		}
		offset += current_chunk_size;
		core_scheduler().reschedule();
	}

	if success {
		info!("AHCI: Boot log written to sector 1024 successfully!");

		// Read it back to verify in 4KB chunks
		offset = 0;
		while offset < fb_size {
			let current_chunk_size = core::cmp::min(chunk_size, fb_size - offset);
			let current_sectors = ((current_chunk_size + 511) / 512) as u16;

			if read_sectors(
				1024 + (offset / 512) as u64,
				current_sectors,
				verify_buf.as_mut_ptr(),
			)
			.is_ok()
			{
				let original =
					unsafe { slice::from_raw_parts(fb_ptr.add(offset), current_chunk_size) };
				if &verify_buf[..current_chunk_size] != original {
					warn!("AHCI: Boot log verification failed at offset {}", offset);
					success = false;
					break;
				}
			} else {
				error!("AHCI: Failed to read back chunk at offset {}", offset);
				success = false;
				break;
			}
			offset += current_chunk_size;
			core_scheduler().reschedule();
		}

		if success {
			info!("AHCI: Boot log verified successfully!");
			// Print first 16 bytes for visual confirmation
			trace!("AHCI: Disk head (LBA 1024): {:02x?}", &verify_buf[0..16]);
		}
	}

	// Unmap framebuffer
	mm::unmap(fb_virt, fb_size);
}

fn ahci_device_type(sig: u32) -> &'static str {
	match sig {
		0x0000_0101 | 0x0101_0101 => "SATA",
		0xeb14_0101 => "SATAPI",
		0xc33c_0101 => "Enclosure Management Bridge",
		0x9669_0101 => "Port Multiplier",
		_ => "Unknown",
	}
}

fn identify_device(port: *mut u32, cl_ptr: *mut CommandHeader) {
	// Allocate necessary DMA memory for identification payload
	let mut ct = Box::new_in(
		CommandTable {
			cfis: [0; 64],
			acmd: [0; 16],
			reserved: [0; 48],
			prdt: [PRDTEntry::default()],
		},
		DeviceAlloc,
	);
	let data = Box::new_in([0u16; 256], DeviceAlloc);

	let ct_phys = mm::virtual_to_physical(VirtAddr::from_ptr(ct.as_ref() as *const _ as *const u8))
		.unwrap()
		.as_u64();
	let data_phys =
		mm::virtual_to_physical(VirtAddr::from_ptr(data.as_ptr() as *const _ as *const u8))
			.unwrap()
			.as_u64();

	start_port(port);

	// Prepare IDENTIFY DEVICE command
	let mut header = CommandHeader::default();
	header.cfl_flags = 5; // FIS length: 5 dwords
	header.prdtl = 1; // 1 PRDT entry
	header.ctba = ct_phys as u32;
	header.ctbau = (ct_phys >> 32) as u32;

	let mut prdt = PRDTEntry::default();
	prdt.dba = data_phys as u32;
	prdt.dbau = (data_phys >> 32) as u32;
	prdt.dbc_flags = 511; // 512 bytes - 1

	ct.cfis[0] = 0x27; // Register H2D FIS
	ct.cfis[1] = 0x80; // Command bit set
	ct.cfis[2] = 0xec; // IDENTIFY DEVICE
	ct.prdt[0] = prdt;

	unsafe {
		// Put header in slot 0
		cl_ptr.write_volatile(header);

		// Issue command
		port.add(14).write_volatile(1); // PxCI bit 0

		// Wait for completion (up to 500ms)
		let mut timeout = 500;
		while port.add(14).read_volatile() & 1 != 0 && timeout > 0 {
			if port.add(4).read_volatile() & (1 << 30) != 0 {
				warn!("AHCI: IDENTIFY DEVICE error occurred (PxIS.TFES)");
				break;
			}
			crate::arch::processor::udelay(1000);
			timeout -= 1;
		}

		if timeout == 0 {
			warn!("AHCI: IDENTIFY DEVICE timed out");
			return;
		}
	}

	// Read specs
	let serial: String = data[10..20]
		.iter()
		.flat_map(|&w| w.to_be_bytes())
		.map(|b| {
			if b.is_ascii_graphic() || b == b' ' {
				b as char
			} else {
				'.'
			}
		})
		.collect();
	let firmware: String = data[23..27]
		.iter()
		.flat_map(|&w| w.to_be_bytes())
		.map(|b| {
			if b.is_ascii_graphic() || b == b' ' {
				b as char
			} else {
				'.'
			}
		})
		.collect();
	let model: String = data[27..47]
		.iter()
		.flat_map(|&w| w.to_be_bytes())
		.map(|b| {
			if b.is_ascii_graphic() || b == b' ' {
				b as char
			} else {
				'.'
			}
		})
		.collect();

	let lba48_supported = (data[83] & (1 << 10)) != 0;
	let capacity_sectors = if lba48_supported {
		let low = u32::from(data[100]) | (u32::from(data[101]) << 16);
		let high = u32::from(data[102]) | (u32::from(data[103]) << 16);
		(u64::from(high) << 32) | u64::from(low)
	} else {
		u64::from(u32::from(data[60]) | (u32::from(data[61]) << 16))
	};
	let _ = SSD_SECTOR_COUNT.set(capacity_sectors);
	let capacity_gb = (capacity_sectors * 512) / (1024 * 1024 * 1024);
	let last_lba = capacity_sectors.saturating_sub(1);
	let raw_header_lba = MODEL_FIXED_CACHE_LBA;
	let raw_data_lba = MODEL_FIXED_CACHE_LBA + MODEL_HEADER_SECTORS;

	info!(
		"AHCI: Model: {}, Serial: {}, FW: {}",
		model.trim(),
		serial.trim(),
		firmware.trim()
	);
	info!(
		"AHCI: Capacity: {} GB ({} sectors)",
		capacity_gb, capacity_sectors
	);
	info!("AHCI: Last usable LBA: {}", last_lba);
	info!(
		"AHCI raw model cache: reserved header LBA {}..{}, data starts at LBA {}",
		raw_header_lba,
		raw_data_lba - 1,
		raw_data_lba
	);
}
