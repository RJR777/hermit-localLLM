use crate::{
    Dma, Result, UsbError, reg,
    ring::{EventRing, PhysMem, Ring, Trb, completion, trb_type},
};

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::hint::spin_loop;
use spin::Mutex;

const MMIO_INIT_SIZE: usize = 0x1000;
const CMD_RING_SIZE: usize = 256;
const EVENT_RING_SIZE: usize = 256;
const MAX_ECAP_STEPS: usize = 64;
const SKIP_LEGACY_BIOS_HANDOFF: bool = false;

#[derive(Clone, Copy, Debug)]
struct SupportedProtocol {
    compatible_port_offset: u8,
    compatible_port_count: u8,
    slot_type: u8,
}

/// xHCI Controller
pub struct XhciCtrl<H: Dma> {
    mmio: usize,
    mmio_size: usize,
    cap_length: u8,
    op_base: usize,
    rt_base: usize,
    db_offset: u32,
    max_slots: u8,
    max_ports: u8,
    supported_protocols: Vec<SupportedProtocol>,
    dcbaa: PhysMem<H>,
    scratchpad: Option<PhysMem<H>>,
    cmd_ring: Mutex<Box<Ring<H>>>,
    event_ring: Mutex<Box<EventRing<H>>>,
    pending_events: Mutex<VecDeque<Trb>>,
    host: Arc<H>,
}

impl<H: Dma> XhciCtrl<H> {
    /// Create and initialize a new xHCI controller
    pub fn new(mmio_phys: usize, host: H) -> Result<Self> {
        let host = Arc::new(host);

        // Initial map to read capability registers
        let init_mmio =
            unsafe { host.map_mmio(mmio_phys, MMIO_INIT_SIZE) }.ok_or(UsbError::MapFail)?;
        let cap_length = unsafe { (init_mmio as *const u8).read_volatile() };

        let hcs1: u32 = unsafe { ((init_mmio + reg::HCSPARAMS1) as *const u32).read_volatile() };
        let hcs2: u32 = unsafe { ((init_mmio + reg::HCSPARAMS2) as *const u32).read_volatile() };
        let db_offset: u32 = unsafe { ((init_mmio + reg::DBOFF) as *const u32).read_volatile() };
        let rts_offset: u32 = unsafe { ((init_mmio + reg::RTSOFF) as *const u32).read_volatile() };
        let hcc1: u32 = unsafe { ((init_mmio + reg::HCCPARAMS1) as *const u32).read_volatile() };
        let xecp = (hcc1 >> 16) as usize * 4;

        let max_slots = (hcs1 & 0xff) as u8;
        let max_ports = ((hcs1 >> 24) & 0xff) as u8;
        let max_scratchpad = ((hcs2 >> 27) & 0x1f) | (((hcs2 >> 21) & 0x1f) << 5);
        info!(
            "xHCI: cap_length {:#x}, max_slots {}, max_ports {}, xECP {:#x}, max_scratchpad {}",
            cap_length, max_slots, max_ports, xecp, max_scratchpad
        );
        let mut supported_protocols = Vec::new();

        // Perform BIOS handoff and collect protocol capabilities if xECP is present.
        if xecp != 0 {
            let mut offset = xecp;
            let mut steps = 0usize;
            while offset >= 0x40 && offset + core::mem::size_of::<u32>() <= MMIO_INIT_SIZE {
                steps += 1;
                if steps > MAX_ECAP_STEPS {
                    warn!(
                        "xHCI: Aborting extended capability walk after {} steps",
                        MAX_ECAP_STEPS
                    );
                    break;
                }

                let cap: u32 = unsafe { ((init_mmio + offset) as *const u32).read_volatile() };
                let cap_id = (cap & 0xff) as u8;
                info!(
                    "xHCI: Found Extended Cap ID {} at offset {:#x}",
                    cap_id, offset
                );

                match cap_id {
                    reg::ECAP_USB_LEGACY => {
                        info!(
                            "xHCI: Found Legacy Support Capability at offset {:#x}",
                            offset
                        );
                        if SKIP_LEGACY_BIOS_HANDOFF {
                            warn!("xHCI: Skipping legacy BIOS handoff");
                        } else {
                            info!("xHCI: Claiming OS ownership...");

                            // Claim OS ownership
                            unsafe {
                                let ptr = (init_mmio + offset) as *mut u32;
                                ptr.write_volatile(cap | (1 << 24)); // Set OS Ownership bit

                                // Wait for BIOS to release (max 250ms)
                                let mut lc = 0;
                                while (ptr.read_volatile() & (1 << 16)) != 0 {
                                    lc += 1;
                                    if lc > 250_000 {
                                        error!("xHCI: BIOS failed to release ownership!");
                                        break;
                                    }
                                    spin_loop();
                                }

                                // Clear SMIs in USBLEGCTLSTS
                                let sts_ptr = (init_mmio + offset + 4) as *mut u32;
                                sts_ptr.write_volatile(0xe0000000); // Clear SMI enable bits
                            }
                        }
                    }
                    reg::ECAP_SUPPORTED_PROTOCOL => {
                        let name =
                            unsafe { ((init_mmio + offset + 4) as *const u32).read_volatile() };
                        let ports =
                            unsafe { ((init_mmio + offset + 8) as *const u32).read_volatile() };
                        let slot =
                            unsafe { ((init_mmio + offset + 12) as *const u32).read_volatile() };
                        let compatible_port_offset = (ports & 0xff) as u8;
                        let compatible_port_count = ((ports >> 8) & 0xff) as u8;
                        let slot_type = (slot & 0x1f) as u8;
                        info!(
                            "xHCI: Supported Protocol name={:#010x} port_offset={} port_count={} slot_type={}",
                            name, compatible_port_offset, compatible_port_count, slot_type
                        );
                        if compatible_port_offset != 0 && compatible_port_count != 0 {
                            supported_protocols.push(SupportedProtocol {
                                compatible_port_offset,
                                compatible_port_count,
                                slot_type,
                            });
                        }
                    }
                    _ => {}
                }

                let next = ((cap >> 8) & 0xff) as usize * 4;
                if next == 0 {
                    break;
                }
                if next < 4 {
                    warn!(
                        "xHCI: Invalid extended capability next pointer {:#x} at offset {:#x}",
                        next, offset
                    );
                    break;
                }
                offset += next;
            }

            let _ = offset;
        }

        // Calculate total MMIO size needed
        let mmio_size = (rts_offset as usize + 0x20 + 0x20)
            .max(db_offset as usize + (max_slots as usize + 1) * 4)
            .max(0x10000);

        unsafe {
            host.unmap_mmio(init_mmio, MMIO_INIT_SIZE);
        }

        // Remap with full size
        let mmio = unsafe { host.map_mmio(mmio_phys, mmio_size) }.ok_or(UsbError::MapFail)?;

        let op_base = mmio + cap_length as usize;
        let rt_base = mmio + rts_offset as usize;
        // Allocate DCBAA (Device Context Base Address Array)
        // xHCI spec requires 64-byte alignment for DCBAA
        let dcbaa = PhysMem::alloc(&*host, (max_slots as usize + 1) * 8, 64)?;

        // Allocate scratchpad if needed
        let scratchpad = if max_scratchpad > 0 {
            // xHCI spec requires 64-byte alignment for scratchpad array
            let sp_array = PhysMem::alloc(&*host, max_scratchpad as usize * 8, 64)?;
            // Scratchpad buffers must be page-aligned
            let sp_bufs = PhysMem::alloc(
                &*host,
                max_scratchpad as usize * host.page_size(),
                host.page_size(),
            )?;

            // Fill scratchpad array with buffer addresses
            let array_ptr = sp_array.as_ptr::<u64>();
            for i in 0..max_scratchpad as usize {
                let buf_phys = sp_bufs.phys(&*host) + (i * host.page_size()) as u64;
                unsafe {
                    array_ptr.add(i).write_volatile(buf_phys);
                }
            }

            // Point DCBAA[0] to scratchpad array
            unsafe {
                dcbaa.as_ptr::<u64>().write_volatile(sp_array.phys(&*host));
            }

            // Keep sp_bufs alive, sp_array is referenced via DCBAA[0]
            Some(sp_bufs)
        } else {
            None
        };

        // Allocate rings on heap to reduce stack usage
        let cmd_ring = Box::new(Ring::new(&*host, CMD_RING_SIZE)?);
        let event_ring = Box::new(EventRing::new(&*host, EVENT_RING_SIZE)?);

        let mut ctrl = Self {
            mmio,
            mmio_size,
            cap_length,
            op_base,
            rt_base,
            db_offset,
            max_slots,
            max_ports,
            supported_protocols,
            dcbaa,
            scratchpad,
            cmd_ring: Mutex::new(cmd_ring),
            event_ring: Mutex::new(event_ring),
            pending_events: Mutex::new(VecDeque::new()),
            host,
        };

        ctrl.init()?;
        Ok(ctrl)
    }

    fn event_type_name(ty: u8) -> &'static str {
        match ty as u32 {
            trb_type::TRANSFER_EVENT => "TRANSFER_EVENT",
            trb_type::COMMAND_COMPLETION => "COMMAND_COMPLETION",
            trb_type::PORT_STATUS_CHANGE => "PORT_STATUS_CHANGE",
            trb_type::BANDWIDTH_REQUEST => "BANDWIDTH_REQUEST",
            trb_type::DOORBELL_EVENT => "DOORBELL_EVENT",
            trb_type::HOST_CONTROLLER_EVENT => "HOST_CONTROLLER_EVENT",
            trb_type::DEVICE_NOTIFICATION => "DEVICE_NOTIFICATION",
            trb_type::MFINDEX_WRAP => "MFINDEX_WRAP",
            _ => "UNKNOWN",
        }
    }

    fn init(&mut self) -> Result<()> {
        // Stop controller if running
        let usbcmd = self.read_op::<u32>(reg::USBCMD);
        if (usbcmd & reg::USBCMD_RUN) != 0 {
            self.write_op(reg::USBCMD, usbcmd & !reg::USBCMD_RUN);
            let mut lc = 0;
            while (self.read_op::<u32>(reg::USBSTS) & reg::USBSTS_HCH) == 0 {
                lc += 1;
                if lc > 250_000 {
                    return Err(UsbError::Timeout("init_host wait for HCH == 0"));
                }
                spin_loop();
            }
        }

        // Reset controller
        self.write_op(reg::USBCMD, reg::USBCMD_HCRST);
        let mut lc = 0;
        while (self.read_op::<u32>(reg::USBCMD) & reg::USBCMD_HCRST) != 0 {
            lc += 1;
            if lc > 250_000 {
                return Err(UsbError::Timeout("init_host wait for HCRST to clear"));
            }
            spin_loop();
        }
        lc = 0;
        while (self.read_op::<u32>(reg::USBSTS) & reg::USBSTS_CNR) != 0 {
            lc += 1;
            if lc > 250_000 {
                return Err(UsbError::Timeout("init_host wait for CNR == 0"));
            }
            spin_loop();
        }

        // Configure controller
        self.write_op(reg::CONFIG, self.max_slots as u32);
        self.write_op(reg::DCBAAP, self.dcbaa.phys(&*self.host));

        // Setup command ring
        let cmd_ring = self.cmd_ring.lock();
        let crcr = cmd_ring.phys(&*self.host) | 1; // RCS = 1
        self.write_op(reg::CRCR, crcr);
        drop(cmd_ring);

        // Setup event ring
        let event_ring = self.event_ring.lock();
        let int_base = reg::interrupter_base(self.rt_base as u32 - self.mmio as u32, 0);

        self.write_reg(int_base + reg::ERSTSZ, 1u32);
        self.write_reg(int_base + reg::ERSTBA, event_ring.erst_phys(&*self.host));
        self.write_reg(int_base + reg::ERDP, event_ring.ring_phys(&*self.host));
        drop(event_ring);

        // Enable interrupts and start controller
        self.write_op(reg::USBCMD, reg::USBCMD_RUN | reg::USBCMD_INTE);

        // Wait for controller to be ready
        let mut lc = 0;
        while (self.read_op::<u32>(reg::USBSTS) & reg::USBSTS_HCH) != 0 {
            lc += 1;
            if lc > 250_000 {
                return Err(UsbError::Timeout("init_host wait for final RUN HCH == 0"));
            }
            spin_loop();
        }

        Ok(())
    }

    fn read_reg<T: Copy + PartialEq + core::fmt::Debug>(&self, offset: usize) -> T {
        unsafe {
            if core::mem::size_of::<T>() == 8 {
                let ptr = (self.mmio + offset) as *const u32;
                let low = ptr.read_volatile();
                let high = ptr.add(1).read_volatile();
                let val_u64 = ((high as u64) << 32) | (low as u64);
                let mut val = core::mem::MaybeUninit::<T>::uninit();
                core::ptr::copy_nonoverlapping(
                    &val_u64 as *const u64 as *const u8,
                    val.as_mut_ptr() as *mut u8,
                    8,
                );
                val.assume_init()
            } else {
                ((self.mmio + offset) as *const T).read_volatile()
            }
        }
    }

    fn write_reg<T: Copy + PartialEq + core::fmt::Debug>(&self, offset: usize, val: T) {
        unsafe {
            if core::mem::size_of::<T>() == 8 {
                let ptr = (self.mmio + offset) as *mut u64;
                let val_u64 = *(&val as *const _ as *const u64);
                ptr.write_volatile(val_u64);
            } else {
                ((self.mmio + offset) as *mut T).write_volatile(val);
            }
        }
    }

    fn read_op<T: Copy + PartialEq + core::fmt::Debug>(&self, offset: usize) -> T {
        self.read_reg(self.op_base - self.mmio + offset)
    }

    fn write_op<T: Copy + PartialEq + core::fmt::Debug>(&self, offset: usize, val: T) {
        self.write_reg(self.op_base - self.mmio + offset, val)
    }

    /// Ring the command doorbell
    fn ring_cmd_doorbell(&self) {
        let db = reg::doorbell(self.db_offset, 0);
        self.write_reg(db, 0u32);
    }

    /// Ring device doorbell
    pub fn ring_doorbell(&self, slot: u8, target: u8) {
        let db = reg::doorbell(self.db_offset, slot);
        let _ = slot;
        self.write_reg(db, target as u32);
    }

    /// Update event ring dequeue pointer
    fn update_erdp(&self) {
        let event_ring = self.event_ring.lock();
        let int_base = reg::interrupter_base(self.rt_base as u32 - self.mmio as u32, 0);
        self.write_reg(
            int_base + reg::ERDP,
            event_ring.dequeue_ptr(&*self.host) | 0x8,
        );
    }

    /// Wait for command completion
    pub fn wait_command(&self) -> Result<Trb> {
        let mut loop_counter = 0usize;
        loop {
            loop_counter += 1;

            // Check for fatal errors every 100k loops
            if loop_counter % 100_000 == 0 {
                let status: u32 = self.read_op(reg::USBSTS);
                if (status & reg::USBSTS_HSE) != 0 {
                    error!(
                        "xHCI: FATAL - Host System Error detected in USBSTS: {:#x}",
                        status
                    );
                    return Err(UsbError::XferFail(status as u8));
                }
            }

            if loop_counter > 250_000 {
                let status: u32 = self.read_op(reg::USBSTS);
                let crcr: u64 = self.read_op(reg::CRCR);
                error!(
                    "xHCI: wait_command TIMEOUT! USBSTS: {:#x}, CRCR: {:#x}",
                    status, crcr
                );
                return Err(UsbError::Timeout(
                    "wait_command waiting for xHC command completion",
                ));
            }

            let trb = {
                let mut event_ring = self.event_ring.lock();
                event_ring.try_dequeue()
            };

            if let Some(trb) = trb {
                self.update_erdp();

                match trb.trb_type() as u32 {
                    trb_type::COMMAND_COMPLETION => {
                        let code = trb.completion_code();
                        if code != completion::SUCCESS {
                            info!(
                                "xHCI: Command failed with code: {} ({})",
                                code,
                                completion::name(code)
                            );
                            return Err(UsbError::CmdFail(code));
                        }
                        return Ok(trb);
                    }
                    trb_type::PORT_STATUS_CHANGE => {
                        continue;
                    }
                    trb_type::TRANSFER_EVENT => {
                        let code = trb.completion_code();
                        if code != completion::SUCCESS && code != completion::SHORT_PACKET {
                            info!(
                                "xHCI: Transfer event while waiting for command slot={} ep={} code={} ({}) param={:#x} status={:#x} control={:#x}",
                                trb.slot_id(),
                                trb.endpoint_id(),
                                code,
                                completion::name(code),
                                trb.param,
                                trb.status,
                                trb.control
                            );
                        }
                        continue;
                    }
                    _ => {
                        info!(
                            "xHCI: Event type {} ({}) on wait_command slot={} ep={} code={} ({}) param={:#x} status={:#x} control={:#x}",
                            trb.trb_type(),
                            Self::event_type_name(trb.trb_type()),
                            trb.slot_id(),
                            trb.endpoint_id(),
                            trb.completion_code(),
                            completion::name(trb.completion_code()),
                            trb.param,
                            trb.status,
                            trb.control
                        );
                    }
                }
            }

            spin_loop();
        }
    }

    fn poll_raw_event(&self) -> Option<Trb> {
        let mut event_ring = self.event_ring.lock();
        let trb = event_ring.try_dequeue();
        drop(event_ring);
        if trb.is_some() {
            self.update_erdp();
        }
        trb
    }

    /// Poll for the next pending or hardware event without filtering.
    pub fn poll_event(&self) -> Option<Trb> {
        if let Some(trb) = self.pending_events.lock().pop_front() {
            return Some(trb);
        }

        self.poll_raw_event()
    }

    /// Poll for an event matching `matches`, preserving unrelated events.
    pub fn poll_event_matching(&self, mut matches: impl FnMut(&Trb) -> bool) -> Option<Trb> {
        {
            let mut pending = self.pending_events.lock();
            if let Some(pos) = pending.iter().position(&mut matches) {
                return pending.remove(pos);
            }
        }

        while let Some(trb) = self.poll_raw_event() {
            if matches(&trb) {
                return Some(trb);
            }

            self.pending_events.lock().push_back(trb);
        }

        None
    }

    /// Submit a command TRB
    pub fn submit_command(&self, trb: Trb) -> Result<Trb> {
        let mut cmd_ring = self.cmd_ring.lock();
        cmd_ring.enqueue(&*self.host, trb);
        drop(cmd_ring);
        self.ring_cmd_doorbell();
        self.wait_command()
    }

    fn slot_type_for_port(&self, port: u8) -> u8 {
        let root_port = port + 1;
        self.supported_protocols
            .iter()
            .find(|protocol| {
                let first = protocol.compatible_port_offset;
                let last = first.saturating_add(protocol.compatible_port_count);
                root_port >= first && root_port < last
            })
            .map(|protocol| protocol.slot_type)
            .unwrap_or(0)
    }

    /// Enable a device slot
    pub fn enable_slot(&self) -> Result<u8> {
        self.enable_slot_with_type(0)
    }

    /// Enable a device slot for the protocol associated with a zero-based root port.
    pub fn enable_slot_for_port(&self, port: u8) -> Result<u8> {
        let slot_type = self.slot_type_for_port(port);
        info!(
            "xHCI: Enable Slot for zero_based_port={} root_port={} slot_type={}",
            port,
            port + 1,
            slot_type
        );
        self.enable_slot_with_type(slot_type)
    }

    /// Enable a device slot with an xHCI Supported Protocol Slot Type.
    pub fn enable_slot_with_type(&self, slot_type: u8) -> Result<u8> {
        let trb = Trb {
            param: 0,
            status: 0,
            control: (trb_type::ENABLE_SLOT << 10) | (((slot_type as u32) & 0x1f) << 16),
        };
        let evt = self.submit_command(trb)?;
        Ok(evt.slot_id())
    }

    /// Disable a device slot
    pub fn disable_slot(&self, slot_id: u8) -> Result<()> {
        let trb = Trb {
            param: 0,
            status: 0,
            control: (trb_type::DISABLE_SLOT << 10) | ((slot_id as u32) << 24),
        };
        self.submit_command(trb)?;
        Ok(())
    }

    /// Read port status
    pub fn port_status(&self, port: u8) -> u32 {
        let offset = reg::port_reg_base(self.cap_length, port);
        self.read_reg(offset)
    }

    /// Write port status (for clearing change bits, reset, etc.)
    pub fn write_port_status(&self, port: u8, val: u32) {
        let offset = reg::port_reg_base(self.cap_length, port);
        self.write_reg(offset, val);
    }

    /// Acknowledge any latched RW1C port change bits.
    pub fn clear_port_change_bits(&self, port: u8) {
        let portsc = self.port_status(port);
        let change_bits = portsc
            & (reg::PORTSC_CSC
                | reg::PORTSC_PEC
                | reg::PORTSC_WRC
                | reg::PORTSC_OCC
                | reg::PORTSC_PRC
                | reg::PORTSC_PLC
                | reg::PORTSC_CEC);
        if change_bits != 0 {
            self.write_port_status(port, (portsc & reg::PORTSC_PP) | change_bits);
        }
    }

    /// Ensure a root port is powered. Some bare-metal firmware paths leave xHC
    /// root ports unpowered after controller reset until the OS asserts PP.
    pub fn power_on_port(&self, port: u8) {
        let portsc = self.port_status(port);
        if (portsc & reg::PORTSC_PP) != 0 {
            return;
        }

        let change_bits = portsc
            & (reg::PORTSC_CSC
                | reg::PORTSC_PEC
                | reg::PORTSC_WRC
                | reg::PORTSC_OCC
                | reg::PORTSC_PRC
                | reg::PORTSC_PLC
                | reg::PORTSC_CEC);
        self.write_port_status(port, reg::PORTSC_PP | change_bits);
    }

    /// Reset a port
    pub fn reset_port(&self, port: u8) -> Result<()> {
        let offset = reg::port_reg_base(self.cap_length, port);
        let portsc: u32 = self.read_reg(offset);
        if (portsc & reg::PORTSC_PP) == 0 {
            self.power_on_port(port);
        }

        // Set port reset, preserve PP, clear change bits
        let portsc: u32 = self.read_reg(offset);
        let val = (portsc & reg::PORTSC_PP) | reg::PORTSC_PR;
        self.write_reg(offset, val);

        // Wait for reset to complete
        let mut loop_counter = 0usize;
        loop {
            loop_counter += 1;
            if loop_counter > 250_000 {
                return Err(UsbError::Timeout("reset_port waiting for xHC PORTSC_PRC"));
            }

            let portsc: u32 = self.read_reg(offset);
            if (portsc & reg::PORTSC_PRC) != 0 {
                // Clear PRC by writing 1
                let val = (portsc & reg::PORTSC_PP) | reg::PORTSC_PRC;
                self.write_reg(offset, val);
                break;
            }
            spin_loop();
        }

        let mut loop_counter = 0usize;
        loop {
            let portsc: u32 = self.read_reg(offset);
            if (portsc & reg::PORTSC_CCS) == 0 || (portsc & reg::PORTSC_PED) != 0 {
                break;
            }

            loop_counter += 1;
            if loop_counter > 250_000 {
                warn!(
                    "xHCI: port {} reset completed but PED did not set, status={:#x}",
                    port, portsc
                );
                break;
            }
            spin_loop();
        }

        Ok(())
    }

    /// Get port speed (after device is connected and port is enabled)
    pub fn port_speed(&self, port: u8) -> u8 {
        let portsc = self.port_status(port);
        ((portsc >> 10) & 0xf) as u8
    }

    /// Check if device is connected on port
    pub fn port_connected(&self, port: u8) -> bool {
        (self.port_status(port) & reg::PORTSC_CCS) != 0
    }

    /// Set device context in DCBAA
    pub fn set_device_context(&self, slot: u8, phys: u64) {
        unsafe {
            self.dcbaa
                .as_ptr::<u64>()
                .add(slot as usize)
                .write_volatile(phys);
        }
    }

    /// Get host reference
    pub fn host(&self) -> &H {
        &self.host
    }

    /// Get max slots
    pub fn max_slots(&self) -> u8 {
        self.max_slots
    }

    /// Get max ports
    pub fn max_ports(&self) -> u8 {
        self.max_ports
    }

    /// Push an event back into the pending queue.
    pub fn push_pending_event(&self, trb: Trb) {
        self.pending_events.lock().push_back(trb);
    }

    /// Poll for events from the controller.
    pub fn poll(&self) {
        while let Some(_) = self.poll_event() {}
    }

    /// Read USBSTS register.
    pub fn usb_status(&self) -> u32 {
        self.read_op(reg::USBSTS)
    }

    /// Read USBCMD register.
    pub fn usb_command(&self) -> u32 {
        self.read_op(reg::USBCMD)
    }
}

impl<H: Dma> Drop for XhciCtrl<H> {
    fn drop(&mut self) {
        // Stop controller
        let usbcmd = self.read_op::<u32>(reg::USBCMD);
        self.write_op(reg::USBCMD, usbcmd & !reg::USBCMD_RUN);

        // Wait for halt
        while (self.read_op::<u32>(reg::USBSTS) & reg::USBSTS_HCH) == 0 {
            spin_loop();
        }

        // Unmap MMIO
        unsafe {
            self.host.unmap_mmio(self.mmio, self.mmio_size);
        }
    }
}
