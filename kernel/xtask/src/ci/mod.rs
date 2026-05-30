use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;

mod c;
mod firecracker;
mod qemu;
mod rs;
mod uhyve;

/// Run CI tasks.
#[derive(Subcommand)]
pub enum Ci {
	C(c::C),
	Rs(rs::Rs),
}

impl Ci {
	pub fn run(self) -> Result<()> {
		match self {
			Self::C(c) => c.run(),
			Self::Rs(rs) => rs.run(),
		}
	}
}

fn in_ci() -> bool {
	std::env::var_os("CI") == Some("true".into())
}

pub fn parent_root() -> PathBuf {
	crate::project_root()
		.parent()
		.unwrap_or_else(|| Path::new("."))
		.to_path_buf()
}
