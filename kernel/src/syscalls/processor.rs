use crate::arch::get_processor_count;

/// Returns the number of processors currently online.
#[hermit_macro::system]
#[unsafe(no_mangle)]
pub extern "C" fn sys_get_processor_count() -> usize {
	get_processor_count().try_into().unwrap()
}

#[hermit_macro::system]
#[unsafe(no_mangle)]
pub extern "C" fn sys_available_parallelism() -> usize {
	get_processor_count().try_into().unwrap()
}

/// Returns the processor frequency in MHz.
#[hermit_macro::system]
#[unsafe(no_mangle)]
pub extern "C" fn sys_get_processor_frequency() -> u16 {
	crate::arch::processor::get_frequency()
}

/// Returns the processor frequency override in MHz, or 0 if no override was provided.
#[hermit_macro::system]
#[unsafe(no_mangle)]
pub extern "C" fn sys_get_processor_frequency_override() -> u16 {
	crate::env::freq().unwrap_or(0)
}

#[hermit_macro::system]
#[unsafe(no_mangle)]
pub extern "C" fn sys_get_timer_ticks() -> u64 {
	crate::arch::processor::get_timer_ticks()
}
