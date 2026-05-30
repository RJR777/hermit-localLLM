//! Central parsing of the command-line parameters.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::Ordering;
use core::{ptr, str};

use ahash::RandomState;
use fdt::Fdt;
use hashbrown::HashMap;
use hashbrown::hash_map::Iter;
use hermit_entry::boot_info::{BootInfo, RawBootInfo};
use hermit_sync::OnceCell;

pub(crate) use crate::arch::kernel::{self, get_base_address, get_image_size, get_ram_address};

static BOOT_INFO: OnceCell<BootInfo> = OnceCell::new();
const VIRTUAL_CONSOLE_PAGE_SIZE: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FramebufferFormat {
	Rgb,
	Bgr,
	Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
	pub address: usize,
	pub width: usize,
	pub height: usize,
	pub stride: usize,
	pub format: FramebufferFormat,
}

pub fn boot_info() -> &'static BootInfo {
	BOOT_INFO.get().unwrap()
}

pub fn set_boot_info(raw_boot_info: RawBootInfo) {
	let boot_info = BootInfo::from(raw_boot_info);
	BOOT_INFO.set(boot_info).unwrap();
}

static CLI: OnceCell<Cli> = OnceCell::new();

pub fn init() {
	CLI.set(Cli::default()).unwrap();
}

#[derive(Debug)]
struct Cli {
	#[allow(dead_code)]
	image_path: Option<String>,
	#[cfg(not(target_arch = "riscv64"))]
	freq: Option<u16>,
	env_vars: HashMap<String, String, RandomState>,
	args: Vec<String>,
	#[allow(dead_code)]
	mmio: Vec<String>,
}

/// Whether Hermit is running under the "uhyve" hypervisor.
pub fn is_uhyve() -> bool {
	false
}

pub fn is_uefi() -> bool {
	fdt().is_some_and(|fdt| fdt.root().compatible().first() == "hermit,uefi")
}

pub fn fdt() -> Option<Fdt<'static>> {
	boot_info().hardware_info.device_tree.map(|fdt| {
		let ptr = ptr::with_exposed_provenance(fdt.get().try_into().unwrap());
		unsafe { Fdt::from_ptr(ptr).unwrap() }
	})
}

/// Returns the RSDP physical address if available.
#[cfg(all(target_arch = "x86_64", feature = "acpi"))]
pub fn rsdp() -> Option<core::num::NonZero<usize>> {
	let rsdp = fdt()?
		.find_node("/hermit,rsdp")?
		.reg()?
		.next()?
		.starting_address
		.addr();
	core::num::NonZero::new(rsdp)
}

pub fn fdt_args() -> Option<&'static str> {
	fdt().and_then(|fdt| fdt.chosen().bootargs())
}

#[derive(Clone, Copy, Debug)]
pub struct VirtualConsoleInfo {
	pub address: NonZeroU64,
	pub size: u64,
}

pub fn virtual_console_info() -> Option<VirtualConsoleInfo> {
	let reg = fdt()?
		.find_compatible(&["hermit,debugcon-v1"])?
		.reg()?
		.next()?;
	let address = NonZeroU64::new(reg.starting_address.addr() as u64)?;
	let size = reg.size.unwrap_or(VIRTUAL_CONSOLE_PAGE_SIZE) as u64;

	Some(VirtualConsoleInfo { address, size })
}

pub fn framebuffer_info() -> Option<FramebufferInfo> {
	let fdt = fdt()?;
	let node = fdt.find_compatible(&["simple-framebuffer"])?;
	let reg = node.reg()?.next()?;
	let width = node.property("width")?.as_usize()?;
	let height = node.property("height")?.as_usize()?;
	let mut address = reg.starting_address.addr();
	let mut stride = node.property("stride")?.as_usize()?;
	let format = match node.property("format")?.as_str()? {
		"r8g8b8x8" => FramebufferFormat::Rgb,
		"b8g8r8x8" => FramebufferFormat::Bgr,
		_ => FramebufferFormat::Unknown,
	};

	#[cfg(target_arch = "x86_64")]
	{
		let loader_address = kernel::LOADER_FB_BASE.load(Ordering::Relaxed);
		let loader_stride = kernel::LOADER_FB_STRIDE.load(Ordering::Relaxed);
		if loader_address != 0 {
			address = loader_address as usize;
		}
		if loader_stride != 0 {
			stride = loader_stride as usize;
		}
	}

	Some(FramebufferInfo {
		address,
		width,
		height,
		stride,
		format,
	})
}

use crate::init_cell::InitCell;

#[derive(Clone)]
pub struct ModelInfo {
	pub address: usize,
	pub size: usize,
	pub source: &'static str,
}

static KERNEL_MODEL_INFO: InitCell<ModelInfo> = InitCell::new(ModelInfo {
	address: 0,
	size: 0,
	source: "none",
});

pub fn set_model_info(address: usize, size: usize) {
	set_model_info_with_source(address, size, "unknown");
}

pub fn set_model_info_with_source(address: usize, size: usize, source: &'static str) {
	KERNEL_MODEL_INFO.with(|info: Option<&mut ModelInfo>| {
		if let Some(info) = info {
			info.address = address;
			info.size = size;
			info.source = source;
		}
	});
}

pub fn model_info() -> Option<ModelInfo> {
	let mut res = None;
	KERNEL_MODEL_INFO.with(|info: Option<&mut ModelInfo>| {
		if let Some(info) = info {
			if info.address != 0 {
				res = Some(info.clone());
			}
		}
	});
	res.or_else(|| {
		let fdt = fdt()?;
		let node = fdt
			.find_compatible(&["hermit,model"])
			.or_else(|| fdt.find_node("/hermit,model"))?;
		let reg = node.reg()?.next()?;
		Some(ModelInfo {
			address: reg.starting_address.addr(),
			size: reg.size.unwrap_or(0),
			source: "fdt",
		})
	})
}

impl Default for Cli {
	fn default() -> Self {
		let mut image_path = None;
		#[cfg(not(target_arch = "riscv64"))]
		let mut freq = None;
		let mut env_vars = HashMap::<String, String, RandomState>::with_hasher(
			RandomState::with_seeds(0, 0, 0, 0),
		);

		let args = kernel::args().or_else(fdt_args).unwrap_or_default();
		info!("bootargs = {args}");
		let words = shell_words::split(args).unwrap();

		let mut words = words.into_iter();
		let expect_arg = |arg: Option<String>, name: &str| {
			arg.unwrap_or_else(|| {
				panic!("The argument '{name}' requires a value but none was supplied")
			})
		};

		let mut args = Vec::new();
		let mut mmio = Vec::new();
		while let Some(word) = words.next() {
			if word.as_str().starts_with("virtio_mmio.device=") {
				let v: Vec<&str> = word.as_str().split('=').collect();
				mmio.push(v[1].to_owned());
				continue;
			}

			match word.as_str() {
				#[cfg(not(target_arch = "riscv64"))]
				"-freq" => {
					let s = expect_arg(words.next(), word.as_str());
					freq = Some(s.parse().unwrap());
				}
				"-ip" => {
					let ip = expect_arg(words.next(), word.as_str());
					env_vars.insert(String::from("HERMIT_IP"), ip);
				}
				"-mask" => {
					let mask = expect_arg(words.next(), word.as_str());
					env_vars.insert(String::from("HERMIT_MASK"), mask);
				}
				"-gateway" => {
					let gateway = expect_arg(words.next(), word.as_str());
					env_vars.insert(String::from("HERMIT_GATEWAY"), gateway);
				}
				"-mount" => {
					let gateway = expect_arg(words.next(), word.as_str());
					env_vars.insert(String::from("UHYVE_MOUNT"), gateway);
				}
				"--" => args.extend(&mut words),
				word if word.contains('=') => {
					let (arg, value) = word.split_once('=').unwrap();

					match arg {
						"env" => {
							let Some((key, value)) = value.split_once('=') else {
								error!("could not parse bootarg: {word}");
								continue;
							};
							env_vars.insert(key.to_owned(), value.to_owned());
						}
						_ => error!("could not parse bootarg: {word}"),
					}
				}
				_ if image_path.is_none() => image_path = Some(word),
				word => error!("could not parse bootarg: {word}"),
			};
		}

		Self {
			image_path,
			#[cfg(not(target_arch = "riscv64"))]
			freq,
			env_vars,
			args,
			#[allow(dead_code)]
			mmio,
		}
	}
}

/// CPU Frequency in MHz if given through the -freq command-line parameter.
#[cfg(not(target_arch = "riscv64"))]
pub fn freq() -> Option<u16> {
	CLI.get().unwrap().freq
}

#[allow(dead_code)]
pub fn var(key: &str) -> Option<&String> {
	CLI.get().unwrap().env_vars.get(key)
}

pub fn vars() -> Iter<'static, String, String> {
	CLI.get().unwrap().env_vars.iter()
}

/// Returns the cmdline argument passed in after "--"
pub fn args() -> &'static [String] {
	CLI.get().unwrap().args.as_slice()
}

/// Returns the configuration of all mmio devices
#[allow(dead_code)]
pub fn mmio() -> &'static [String] {
	CLI.get().unwrap().mmio.as_slice()
}
