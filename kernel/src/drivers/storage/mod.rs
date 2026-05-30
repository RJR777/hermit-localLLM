pub mod ahci;

pub(crate) fn init() {
	ahci::init();
}
