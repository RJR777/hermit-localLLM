mod allocator;
mod console;

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::{ptr, slice};

use align_address::Align;
use hermit_entry::boot_info::{
	BootInfo, DeviceTreeAddress, HardwareInfo, PlatformInfo, SerialPortBase,
};
use hermit_entry::elf::{KernelObject, LoadedKernel};
use log::info;
use uefi::boot::{AllocateType, MemoryType, PAGE_SIZE};
use uefi::prelude::*;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::proto::media::file::{Directory, File, FileAttribute, FileInfo, FileMode};
use uefi::table::cfg::ConfigTableEntry;

pub use self::console::CONSOLE;
use crate::fdt::{Fdt, FramebufferInfo};
use crate::{BootInfoExt, arch};

// Entry Point of the Uefi Loader
#[entry]
fn main() -> Status {
	uefi::helpers::init().unwrap();
	crate::log::init();

	info!("=== HermitOS Bare-Metal Loader ===");

	let mut esp = Esp::new().unwrap();

	info!("Reading kernel application...");
	let kernel_image = esp.read_app();
	let kernel = KernelObject::parse(&kernel_image).unwrap();
	info!("Kernel parsed, mem_size = {:#x}", kernel.mem_size());

	let total_size = kernel.mem_size() + 32_768;
	let start_addr = kernel.start_addr();

	let kernel_memory = if let Some(addr) = start_addr {
		info!(
			"Kernel has fixed start address {:#x}. Requesting fixed allocation...",
			addr
		);
		let size = total_size.align_up(PAGE_SIZE);
		let ptr = boot::allocate_pages(
			AllocateType::Address(addr),
			MemoryType::LOADER_DATA,
			size / PAGE_SIZE,
		)
		.expect("Failed to allocate fixed memory for static kernel at preferred address");
		unsafe { slice::from_raw_parts_mut(ptr.cast().as_ptr(), size) }
	} else {
		alloc_page_slice(total_size).unwrap()
	};

	let kernel_memory = &mut kernel_memory[..kernel.mem_size()];

	let kernel_info = kernel.load_kernel(kernel_memory, kernel_memory.as_ptr() as u64);
	info!("Kernel loaded at {:#x}", kernel_memory.as_ptr() as u64);

	let rsdp = rsdp();
	let framebuffer = framebuffer_info();

	if framebuffer.is_none() {
		info!("WARNING: No framebuffer info available! Display mirroring will not work.");
	}

	drop(kernel_image);

	let mut fdt = Fdt::new("uefi")
		.unwrap()
		.rsdp(u64::try_from(rsdp.expose_provenance()).unwrap())
		.unwrap();

	if let Some(bootargs) = esp.read_bootargs() {
		fdt = fdt.bootargs(bootargs).unwrap();
	}

	info!("UEFI model preload disabled; kernel USB MSC ingestion will load the model.");

	if let Some(framebuffer) = framebuffer.clone() {
		info!(
			"Writing framebuffer info to FDT: addr={:#x}, {}x{}, stride={}, format={}",
			framebuffer.address,
			framebuffer.width,
			framebuffer.height,
			framebuffer.stride,
			framebuffer.format
		);
		fdt = fdt.framebuffer(framebuffer).unwrap();
	}

	info!("Calling exit_boot_services...");
	allocator::exit_boot_services();
	let mut memory_map = unsafe { boot::exit_boot_services(None) };

	let fdt = fdt.memory_map(&mut memory_map).unwrap().finish().unwrap();

	let (fb_base, fb_stride) = framebuffer
		.as_ref()
		.map(|fb| (fb.address, u64::from(fb.stride)))
		.unwrap_or((0, 0));

	unsafe { boot_kernel(kernel_info, fdt, fb_base, fb_stride) }
}

fn framebuffer_info() -> Option<FramebufferInfo> {
	let handle = match boot::get_handle_for_protocol::<GraphicsOutput>() {
		Ok(handle) => handle,
		Err(err) => {
			info!("UEFI GOP framebuffer not found: {err}");
			return None;
		}
	};
	let mut gop = unsafe {
		match boot::open_protocol::<GraphicsOutput>(
			boot::OpenProtocolParams {
				handle,
				agent: boot::image_handle(),
				controller: None,
			},
			boot::OpenProtocolAttributes::GetProtocol,
		) {
			Ok(gop) => gop,
			Err(err) => {
				info!("Could not open UEFI GOP: {err}");
				return None;
			}
		}
	};

	let mode = gop.current_mode_info();
	let (width, height) = mode.resolution();
	let format = match mode.pixel_format() {
		PixelFormat::Rgb => "r8g8b8x8",
		PixelFormat::Bgr => "b8g8r8x8",
		PixelFormat::Bitmask => {
			info!("UEFI GOP bitmask framebuffer format is not supported for console mirroring yet");
			return None;
		}
		PixelFormat::BltOnly => {
			info!("UEFI GOP BLT-only mode has no direct framebuffer");
			return None;
		}
	};
	let stride = mode.stride();
	let mut framebuffer = gop.frame_buffer();
	let address = framebuffer.as_mut_ptr().expose_provenance() as u64;
	let size = framebuffer.size() as u64;

	info!(
		"Found UEFI GOP framebuffer at {address:#x}, size {size:#x}, mode {width}x{height}, stride {stride}, format {format}"
	);

	Some(FramebufferInfo {
		address,
		size,
		width: width.try_into().unwrap(),
		height: height.try_into().unwrap(),
		stride: stride.try_into().unwrap(),
		format,
	})
}

pub unsafe fn boot_kernel(
	kernel_info: LoadedKernel,
	fdt: Vec<u8>,
	fb_base: u64,
	fb_stride: u64,
) -> ! {
	let LoadedKernel {
		load_info,
		entry_point,
	} = kernel_info;

	let device_tree =
		DeviceTreeAddress::new(u64::try_from(fdt.leak().as_ptr().expose_provenance()).unwrap());

	let boot_info = BootInfo {
		hardware_info: HardwareInfo {
			phys_addr_range: 0..0,
			serial_port_base: SerialPortBase::new(arch::SERIAL_IO_PORT),
			device_tree,
		},
		load_info,
		platform_info: PlatformInfo::Fdt,
	};

	let stack = usize::try_from(boot_info.load_info.kernel_image_addr_range.end).unwrap() + 65_536;
	let stack = stack.align_down(PAGE_SIZE);
	let entry = ptr::with_exposed_provenance(entry_point.try_into().unwrap());
	let stack = ptr::with_exposed_provenance_mut(stack);
	let raw_boot_info = boot_info.write();

	unsafe { arch::enter_kernel(stack, entry, raw_boot_info, fb_base, fb_stride) }
}

fn alloc_page_slice(size: usize) -> uefi::Result<&'static mut [MaybeUninit<u8>]> {
	let size = size.align_up(PAGE_SIZE);
	let ptr = boot::allocate_pages(
		AllocateType::AnyPages,
		MemoryType::LOADER_DATA,
		size / PAGE_SIZE,
	)?;
	Ok(unsafe { slice::from_raw_parts_mut(ptr.cast().as_ptr(), size) })
}

/// Returns the RSDP.
///
/// This must be called before exiting boot services.
/// See [5.2.5.2. Finding the RSDP on UEFI Enabled Systems — ACPI Specification 6.5 documentation](https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html#finding-the-rsdp-on-uefi-enabled-systems) for details.
fn rsdp() -> *const c_void {
	system::with_config_table(|config_table| {
		let (rsdp, version) = if let Some(entry) = config_table
			.iter()
			.find(|entry| entry.guid == ConfigTableEntry::ACPI2_GUID)
		{
			(entry.address, 2)
		} else {
			let entry = config_table
				.iter()
				.find(|entry| entry.guid == ConfigTableEntry::ACPI_GUID)
				.unwrap();
			(entry.address, 1)
		};
		info!("Found ACPI {version} RSDP at {rsdp:p}");
		rsdp
	})
}

pub struct Esp {
	root: Directory,
}

impl Esp {
	pub fn new() -> uefi::Result<Self> {
		let image_handle = boot::image_handle();
		let mut sfs = boot::get_image_file_system(image_handle)?;
		let root = sfs.open_volume()?;
		Ok(Self { root })
	}

	pub fn read_app(&mut self) -> Vec<u8> {
		self.read_app_at(cstr16!(r"\EFI\hermit\hermit-app"))
			.or_else(|| self.read_app_at(cstr16!(r"\EFI\BOOT\hermit-app")))
			.unwrap()
	}

	pub fn read_bootargs(&mut self) -> Option<String> {
		self.read_bootargs_at(cstr16!(r"\EFI\hermit\hermit-bootargs"))
			.or_else(|| self.read_bootargs_at(cstr16!(r"\EFI\BOOT\hermit-bootargs")))
	}

	fn read_app_at(&mut self, path: &uefi::CStr16) -> Option<Vec<u8>> {
		let file = self
			.root
			.open(path, FileMode::Read, FileAttribute::empty())
			.ok()?;
		let mut file = file.into_regular_file()?;
		let mut info_buf = [0u8; 256];
		let info = file.get_info::<FileInfo>(&mut info_buf).ok()?;
		let size = info.file_size() as usize;
		let mut data = Vec::with_capacity(size);
		unsafe {
			data.set_len(size);
		}
		file.read(&mut data).ok()?;
		info!("Read Hermit application (size = {} B)", size);
		Some(data)
	}

	fn read_bootargs_at(&mut self, path: &uefi::CStr16) -> Option<String> {
		let file = self
			.root
			.open(path, FileMode::Read, FileAttribute::empty())
			.ok()?;
		let mut file = file.into_regular_file()?;
		let mut info_buf = [0u8; 256];
		let info = file.get_info::<FileInfo>(&mut info_buf).ok()?;
		let size = info.file_size() as usize;
		let mut data = Vec::with_capacity(size);
		unsafe {
			data.set_len(size);
		}
		file.read(&mut data).ok()?;
		let bootargs = String::from_utf8(data).ok()?;
		info!("Read Hermit bootargs: {}", bootargs);
		Some(bootargs)
	}
}
