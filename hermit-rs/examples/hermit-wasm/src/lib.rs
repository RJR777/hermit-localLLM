#![feature(thread_local)]

use std::ffi::OsString;
use std::time::Instant;

#[cfg(target_os = "hermit")]
use hermit as _;
use log::{debug, info};
use wasmtime::error::Context;
use wasmtime::*;

#[cfg(target_os = "hermit")]
mod arch;
#[cfg(target_os = "hermit")]
mod capi;
mod preview1;

pub fn run_preview1(
	module_bytes: &[u8],
	config: &wasmtime::Config,
	#[allow(unused_variables)] module_and_args: &'static [OsString],
) -> Result<()> {
	let engine = Engine::new(config)?;
	debug!("Wasmtime engine is configured as followed: {config:?}");

	// TODO: dirty workaround to get the WebAssembly module into
	// the VM. Find a way to inject the `.wasm` file into the VM
	// using another way
	debug!("Create Module");
	let now = Instant::now();
	let module = Module::new(&engine, module_bytes)?;
	let elapsed = now.elapsed();
	debug!("Time to create module: {} msec", elapsed.as_millis());

	debug!("Create Linker");
	#[allow(unused_mut)]
	let mut linker = Linker::new(&engine);

	{
		let mut imports = module.imports();
		if imports.any(|i| i.module() == "wasi_snapshot_preview1") {
			preview1::init(&mut linker, module_and_args)?;
		}
	}

	// All wasm objects operate within the context of a "store". Each
	// `Store` has a type parameter to store host-specific data, which in
	// this case we're using `4` for.
	let mut store = Store::new(&engine, 4);

	for import in module.imports() {
		if import.module() == "env"
			&& import.name() == "memory"
			&& let ExternType::Memory(memory_ty) = import.ty()
		{
			if memory_ty.is_shared() {
				let maximum = memory_ty
					.maximum()
					.expect("shared wasm memory imports must declare a maximum");
				let memory = SharedMemory::new(
					&engine,
					MemoryType::shared(memory_ty.minimum() as u32, maximum as u32),
				)?;
				linker.define(&store, "env", "memory", memory)?;
			} else {
				let memory = Memory::new(&mut store, memory_ty)?;
				linker.define(&store, "env", "memory", memory)?;
			}
		}
	}

	if module
		.imports()
		.any(|i| i.module() == "wasi" && i.name() == "thread-spawn")
	{
		linker.func_wrap("wasi", "thread-spawn", |arg: i32| -> i32 {
			info!("WASI thread-spawn requested arg={arg}; returning synthetic tid 1");
			1
		})?;
	}

	let instance = linker.instantiate(&mut store, &module)?;

	debug!("Try to find symbol _start");
	let func = instance.get_func(&mut store, "_start").unwrap();

	let ty = func.ty(&store);
	if ty.params().len() > 0 {
		panic!("Currently, _start should not receive arguments");
	}

	// Invoke the function and then afterwards print all the results that came
	// out, if there are any.
	let mut results = vec![Val::null_func_ref(); ty.results().len()];
	let values = Vec::new();
	let invoke_res = func
		.call(&mut store, &values, &mut results)
		.with_context(|| "failed to invoke command default".to_string());

	debug!("Return value of entry point: {invoke_res:?}");

	invoke_res
}
