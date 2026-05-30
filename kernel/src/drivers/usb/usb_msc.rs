use alloc::sync::Arc;
use alloc::vec::Vec;

use usb_oxide::{UsbDevice, MscDevice};

use crate::drivers::usb::xhci::HermitDma;
use crate::{env, info, error};

const USB_MSC_DEFAULT_SECTOR_SIZE: usize = 512;
const USB_MSC_PROGRESS_INTERVAL_MB: usize = 16;

async fn yield_now() {
	struct YieldNow {
		yielded: bool,
	}

	impl core::future::Future for YieldNow {
		type Output = ();

		fn poll(mut self: core::pin::Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> core::task::Poll<Self::Output> {
			if self.yielded {
				core::task::Poll::Ready(())
			} else {
				self.yielded = true;
				cx.waker().wake_by_ref();
				core::task::Poll::Pending
			}
		}
	}

	YieldNow { yielded: false }.await;
}

pub async fn msc_handler(device: Arc<UsbDevice<HermitDma>>, _config_desc: Vec<u8>) {
	info!("USB MSC: Handler started");

	let mut msc = match MscDevice::from_interface(device.clone(), todo!("iface"), todo!("in"), todo!("out")) {
		Ok(m) => m,
		Err(_) => {
			error!("USB MSC: Failed to create MSC device");
			return;
		}
	};
    // ... wait, I should probably just make this file empty or remove it from mod.rs if I'm not using it.
    // The user wants me to finish the integration in xhci.rs.
}
