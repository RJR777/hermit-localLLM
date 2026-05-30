#![doc = include_str!("../README.md")]
#![cfg_attr(
	all(target_os = "hermit", feature = "common-os"),
	feature(thread_local)
)]

#[cfg(all(target_os = "hermit", feature = "common-os"))]
mod syscall;

pub fn get_model_data() -> Option<&'static [u8]> {
	extern "C" {
		fn sys_get_model_ptr() -> *const u8;
		fn sys_get_model_len() -> usize;
	}

	unsafe {
		let ptr = sys_get_model_ptr();
		let len = sys_get_model_len();
		if ptr.is_null() || len == 0 {
			None
		} else {
			Some(core::slice::from_raw_parts(ptr, len))
		}
	}
}

pub fn show_kernel_ram_shell_prompt() {
	extern "C" {
		fn sys_show_kernel_ram_shell_prompt();
	}

	unsafe {
		sys_show_kernel_ram_shell_prompt();
	}
}
