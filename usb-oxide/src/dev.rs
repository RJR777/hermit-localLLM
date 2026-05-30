//! USB device abstraction and context structures.

use crate::{
    Dma, Result, UsbError,
    desc::{ConfigDesc, DeviceDesc, EndpointDesc, SetupPacket, desc_type},
    reg,
    ring::{PhysMem, Ring, Trb, completion, trb_type},
    xhci::XhciCtrl,
};

use alloc::{sync::Arc, vec::Vec};
use core::hint::spin_loop;
use spin::Mutex;

const CONFIG_DESCRIPTOR_RETRIES: usize = 3;
const CONFIG_DESCRIPTOR_RETRY_SPINS: usize = 100_000;

fn log_address_context(
    slot_id: u8,
    port: u8,
    root_port: u8,
    portsc: u32,
    slot: &SlotContext,
    ep0: &EndpointContext,
    input_ctx_phys: u64,
    device_ctx_phys: u64,
) {
    info!(
        "ADDRCTX slot={} port={} root_port={} portsc={:#x} input_ctx={:#x} device_ctx={:#x}",
        slot_id, port, root_port, portsc, input_ctx_phys, device_ctx_phys
    );
    info!(
        "ADDRSLOT slot={} dw0={:#010x} dw1={:#010x} dw2={:#010x} dw3={:#010x}",
        slot_id, slot.dw0, slot.dw1, slot.dw2, slot.dw3
    );
    info!(
        "ADDREP0 slot={} dw0={:#010x} dw1={:#010x} tr_lo={:#010x} tr_hi={:#010x} dw4={:#010x}",
        slot_id, ep0.dw0, ep0.dw1, ep0.tr_dequeue_lo, ep0.tr_dequeue_hi, ep0.dw4
    );
}

/// xHCI Slot Context (32 bytes).
///
/// Contains device-specific information used by the xHCI controller
/// to manage USB device communication.
#[repr(C, align(32))]
#[derive(Clone, Copy, Default)]
pub struct SlotContext {
    /// Route String, Speed, Multi-TT, Hub, Context Entries
    pub dw0: u32,
    /// Max Exit Latency, Root Hub Port Number, Number of Ports
    pub dw1: u32,
    /// Interrupter Target, TTT, TT Port Number, TT Hub Slot ID
    pub dw2: u32,
    /// Device Address, Slot State
    pub dw3: u32,
    _0: [u32; 4],
}

impl SlotContext {
    /// Creates a new Slot Context.
    pub fn new(route: u32, speed: u8, context_entries: u8, root_port: u8) -> Self {
        Self {
            dw0: (route & 0xfffff) | ((speed as u32) << 20) | ((context_entries as u32) << 27),
            dw1: ((root_port as u32) << 16),
            dw2: 0,
            dw3: 0,
            _0: [0; 4],
        }
    }
}

/// xHCI Endpoint Context (32 bytes).
///
/// Defines the characteristics and state of a USB endpoint.
#[repr(C, align(32))]
#[derive(Clone, Copy, Default)]
pub struct EndpointContext {
    /// EP State, Mult, MaxPStreams, LSA, Interval, Max ESIT Payload Hi
    pub dw0: u32,
    /// CErr, EP Type, HID, Max Burst Size, Max Packet Size
    pub dw1: u32,
    /// Transfer Ring Dequeue Pointer Low
    pub tr_dequeue_lo: u32,
    /// Transfer Ring Dequeue Pointer High
    pub tr_dequeue_hi: u32,
    /// Average TRB Length, Max ESIT Payload Lo
    pub dw4: u32,
    _0: [u32; 3],
}

impl EndpointContext {
    /// Creates a new Endpoint Context.
    pub fn new(
        ep_type: u8,
        max_packet_size: u16,
        max_burst: u8,
        interval: u8,
        tr_ptr: u64,
    ) -> Self {
        Self {
            dw0: (interval as u32) << 16,
            dw1: ((3u32) << 1)
                | ((ep_type as u32) << 3)
                | ((max_burst as u32) << 8)
                | ((max_packet_size as u32) << 16),
            tr_dequeue_lo: (tr_ptr as u32) | 1, // DCS = 1
            tr_dequeue_hi: (tr_ptr >> 32) as u32,
            dw4: 8, // Average TRB Length
            _0: [0; 3],
        }
    }
}

/// xHCI Input Context for Address Device / Configure Endpoint commands.
///
/// Used to pass configuration data to the xHCI controller when
/// addressing a device or configuring endpoints.
#[repr(C, align(64))]
#[derive(Default)]
pub struct InputContext {
    /// Input Control Context (drop/add flags)
    pub input_control: [u32; 8],
    /// Slot Context
    pub slot: SlotContext,
    /// Endpoint Contexts (EP0 at index 0, EP1 OUT at 1, EP1 IN at 2, etc.)
    pub endpoints: [EndpointContext; 31],
}

/// xHCI Device Context.
///
/// Output context maintained by the xHCI controller containing
/// the current state of a USB device's slot and endpoints.
#[repr(C, align(64))]
#[derive(Default)]
pub struct DeviceContext {
    /// Slot Context
    pub slot: SlotContext,
    /// Endpoint Contexts
    pub endpoints: [EndpointContext; 31],
}

/// USB Device abstraction.
///
/// Represents an addressed USB device connected to an xHCI controller.
/// Provides methods for control transfers, device enumeration, and
/// endpoint configuration.
pub struct UsbDevice<H: Dma> {
    ctrl: Arc<XhciCtrl<H>>,
    slot_id: u8,
    port: u8,
    speed: u8,
    device_ctx: PhysMem<H>,
    input_ctx: PhysMem<H>,
    ep0_ring: Mutex<Ring<H>>,
    ep_rings: Mutex<Vec<Option<Ring<H>>>>,
    device_desc: Option<DeviceDesc>,
}

impl<H: Dma> UsbDevice<H> {
    fn short_settle(&self) {
        for _ in 0..CONFIG_DESCRIPTOR_RETRY_SPINS {
            spin_loop();
        }
    }

    fn recover_ep0_stall(&self) {
        warn!("CFGRECOVER slot={}", self.slot_id);
        let clear_out = SetupPacket::clear_endpoint_feature(crate::feature::ENDPOINT_HALT, 0x00);
        let _ = self.control_transfer(&clear_out, None);
        let clear_in = SetupPacket::clear_endpoint_feature(crate::feature::ENDPOINT_HALT, 0x80);
        let _ = self.control_transfer(&clear_in, None);
    }

    /// Create and address a new USB device
    pub fn new(ctrl: Arc<XhciCtrl<H>>, port: u8) -> Result<Self> {
        let host = ctrl.host();

        // Enable slot
        info!("xHCI: Enabling slot for port {}...", port);
        let slot_id = ctrl.enable_slot_for_port(port)?;
        info!("xHCI: Enabled slot {}", slot_id);

        // xHCI issues the SET_ADDRESS transaction as part of ADDRESS_DEVICE.
        // For SuperSpeed devices, an extra software port reset here can leave
        // the device in a bad state before that implicit EP0 exchange.
        let initial_status = ctrl.port_status(port);
        let initial_speed = ctrl.port_speed(port);
        let speed = if initial_speed == reg::SPEED_SUPER {
            info!(
                "xHCI: Skipping reset on SuperSpeed port {} (status={:#x}, speed={})",
                port, initial_status, initial_speed
            );
            initial_speed
        } else {
            info!("xHCI: Resetting port {}...", port);
            ctrl.reset_port(port)?;
            let speed = ctrl.port_speed(port);
            info!("xHCI: Port speed: {}", speed);
            speed
        };
        ctrl.clear_port_change_bits(port);
        for _ in 0..CONFIG_DESCRIPTOR_RETRY_SPINS {
            spin_loop();
        }

        // Allocate contexts
        let device_ctx = PhysMem::alloc(
            host,
            core::mem::size_of::<DeviceContext>(),
            core::mem::align_of::<DeviceContext>(),
        )?;
        let input_ctx = PhysMem::alloc(
            host,
            core::mem::size_of::<InputContext>(),
            core::mem::align_of::<InputContext>(),
        )?;

        // Allocate EP0 transfer ring
        let ep0_ring = Ring::new(host, 256)?;

        // Setup Input Context
        let input = input_ctx.as_ptr::<InputContext>();
        let root_port = port + 1;
        unsafe {
            // Add flags: Slot Context (bit 0) + EP0 Context (bit 1)
            (*input).input_control[1] = 0b11;

            // Slot Context
            (*input).slot = SlotContext::new(0, speed, 1, root_port);

            // EP0 Context (Control endpoint)
            let max_packet = match speed {
                reg::SPEED_LOW => 8,
                reg::SPEED_FULL => 8,
                reg::SPEED_HIGH => 64,
                reg::SPEED_SUPER => 512,
                _ => 8,
            };
            (*input).endpoints[0] = EndpointContext::new(
                4, // Control Bidirectional
                max_packet,
                0,
                0,
                ep0_ring.phys(host),
            );
        }

        // Set device context in DCBAA
        ctrl.set_device_context(slot_id, device_ctx.phys(host));

        let pre_addr_portsc = ctrl.port_status(port);
        unsafe {
            log_address_context(
                slot_id,
                port,
                root_port,
                pre_addr_portsc,
                &(*input).slot,
                &(*input).endpoints[0],
                input_ctx.phys(host),
                device_ctx.phys(host),
            );
        }

        // Address Device command
        let trb = Trb {
            param: input_ctx.phys(host),
            status: 0,
            control: (trb_type::ADDRESS_DEVICE << 10) | ((slot_id as u32) << 24),
        };
        info!("xHCI: Addressing device on slot {}...", slot_id);
        if let Err(err) = ctrl.submit_command(trb) {
            info!(
                "ADDRFAIL slot={} port={} portsc_before={:#x} portsc_after={:#x} err={:?}",
                slot_id,
                port,
                pre_addr_portsc,
                ctrl.port_status(port),
                err
            );
            return Err(err);
        }
        info!(
            "ADDROK slot={} port={} portsc_after={:#x}",
            slot_id,
            port,
            ctrl.port_status(port)
        );

        // Allocate ep_rings on heap to reduce stack usage
        let mut ep_rings = Vec::with_capacity(31);
        ep_rings.resize_with(31, || None);

        info!("xHCI: Device addressed successfully.");

        Ok(Self {
            ctrl,
            slot_id,
            port,
            speed,
            device_ctx,
            input_ctx,
            ep0_ring: Mutex::new(ep0_ring),
            ep_rings: Mutex::new(ep_rings),
            device_desc: None,
        })
    }

    /// Perform a control transfer
    pub fn control_transfer(
        &self,
        setup: &SetupPacket,
        mut data: Option<&mut [u8]>,
    ) -> Result<usize> {
        let host = self.ctrl.host();
        let mut ep0_ring = self.ep0_ring.lock();

        let data_dir = (setup.request_type & 0x80) != 0; // true = IN
        let data_len = data.as_ref().map(|d| d.len()).unwrap_or(0);
        let requested_length = setup.length;

        // Allocate data buffer if needed
        // Use 64-byte alignment for DMA efficiency (cache line size)
        let data_buf = if data_len > 0 {
            let buf = PhysMem::alloc(host, data_len, 64)?;
            if !data_dir {
                // OUT: copy data to buffer
                if let Some(ref d) = data {
                    unsafe {
                        core::ptr::copy_nonoverlapping(d.as_ptr(), buf.as_ptr(), d.len());
                    }
                }
            }
            Some(buf)
        } else {
            None
        };

        // Setup Stage TRB
        let setup_trb = Trb {
            param: unsafe { *(setup as *const SetupPacket as *const u64) },
            status: 8, // Transfer length = 8
            control: (trb_type::SETUP << 10)
                | (1 << 6) // IDT (Immediate Data)
                | if data_len > 0 && setup.length > 0 {
                    if data_dir { 3 << 16 } else { 2 << 16 } // TRT: IN or OUT
                } else {
                    0 // No data stage
                },
        };
        let _setup_trb_addr = ep0_ring.enqueue(host, setup_trb);

        // Data Stage TRB (if needed)
        let mut data_trb_addr = None;
        if let Some(ref buf) = data_buf {
            let data_trb = Trb {
                param: buf.phys(host),
                status: setup.length as u32,
                control: (trb_type::DATA << 10)
                    | if data_dir { 1 << 16 } else { 0 } // DIR
                    | (1 << 5), // IOC
            };
            data_trb_addr = Some(ep0_ring.enqueue(host, data_trb));
        }

        // Status Stage TRB
        let status_trb = Trb {
            param: 0,
            status: 0,
            control: (trb_type::STATUS << 10)
                | if data_len > 0 && setup.length > 0 && data_dir { 0 } else { 1 << 16 } // DIR
                | (1 << 5), // IOC
        };
        let status_trb_addr = ep0_ring.enqueue(host, status_trb);

        drop(ep0_ring);

        // Ring doorbell for EP0 (target = 1)
        self.ctrl.ring_doorbell(self.slot_id, 1);

        // Wait for completion
        const EP0_ENDPOINT_ID: u8 = 1;
        let mut loop_counter = 0usize;
        let mut data_transferred = None;
        loop {
            loop_counter += 1;
            if loop_counter > 250_000 {
                return Err(UsbError::Timeout(
                    "control_transfer waiting for TRANSFER_EVENT",
                ));
            }

            if let Some(evt) = self.ctrl.poll_event_matching(|evt| {
                evt.trb_type() == trb_type::TRANSFER_EVENT as u8
                    && evt.slot_id() == self.slot_id
                    && evt.endpoint_id() == EP0_ENDPOINT_ID
            }) {
                let is_data_evt = data_trb_addr == Some(evt.param);
                let is_status_evt = evt.param == status_trb_addr;
                if !is_data_evt && !is_status_evt {
                    warn!(
                        "EP0SKIP slot={} expected_status_trb={:#x} event_trb={:#x} code={} residual={}",
                        self.slot_id,
                        status_trb_addr,
                        evt.param,
                        evt.completion_code(),
                        evt.transfer_length()
                    );
                    continue;
                }

                let code = evt.completion_code();
                match code {
                    completion::SUCCESS | completion::SHORT_PACKET => {
                        if is_data_evt {
                            data_transferred = Some(
                                (setup.length as usize)
                                    .saturating_sub(evt.transfer_length() as usize),
                            );
                            continue;
                        }

                        let transferred = if data_len > 0 {
                            data_transferred.unwrap_or(0)
                        } else {
                            0
                        };

                        // Copy data back for IN transfers
                        // ... (we use the original match but let's copy exactly)
                        if data_dir {
                            if let (Some(buf), Some(d)) = (&data_buf, &mut data) {
                                unsafe {
                                    core::ptr::copy_nonoverlapping(
                                        buf.as_ptr::<u8>(),
                                        d.as_mut_ptr(),
                                        transferred.min(d.len()),
                                    );
                                }
                            }
                        }

                        if let Some(buf) = data_buf {
                            buf.free(host);
                        }
                        return Ok(transferred);
                    }
                    completion::STALL_ERROR => {
                        let request_type = setup.request_type;
                        let request = setup.request;
                        let value = setup.value;
                        let index = setup.index;
                        let length = setup.length;
                        warn!(
                            "EP0STALL slot={} bmRequestType={:#04x} bRequest={:#04x} wValue={:#06x} wIndex={:#06x} wLength={} event(param={:#x} status={:#x} control={:#x})",
                            self.slot_id,
                            request_type,
                            request,
                            value,
                            index,
                            length,
                            evt.param,
                            evt.status,
                            evt.control
                        );
                        if let Some(ref buf) = data_buf {
                            let buf = unsafe { core::ptr::read(buf) };
                            buf.free(host);
                        }
                        return Err(UsbError::Stall);
                    }
                    completion::INVALID => {
                        warn!(
                            "xHCI: ignoring invalid EP0 transfer event on slot {} (param={:#x} status={:#x} control={:#x})",
                            self.slot_id, evt.param, evt.status, evt.control
                        );
                    }
                    _ => {
                        warn!(
                            "EP0FAIL slot={} code={} ({}) residual={} requested={} event(param={:#x} status={:#x} control={:#x})",
                            self.slot_id,
                            code,
                            completion::name(code),
                            evt.transfer_length(),
                            requested_length,
                            evt.param,
                            evt.status,
                            evt.control
                        );
                        if let Some(ref buf) = data_buf {
                            let buf = unsafe { core::ptr::read(buf) };
                            buf.free(host);
                        }
                        return Err(UsbError::XferFail(code));
                    }
                }
            }
            spin_loop();
        }
    }

    /// Get device descriptor
    pub fn get_device_descriptor(&mut self) -> Result<DeviceDesc> {
        let mut buf = [0u8; 18];
        let setup = SetupPacket::get_descriptor(desc_type::DEVICE, 0, 18);
        self.control_transfer(&setup, Some(&mut buf))?;

        let desc = unsafe { *(buf.as_ptr() as *const DeviceDesc) };
        self.device_desc = Some(desc);
        Ok(desc)
    }

    /// Get configuration descriptor (full, with interfaces and endpoints)
    pub fn get_config_descriptor(&self, index: u8) -> Result<Vec<u8>> {
        for cycle in 1..=CONFIG_DESCRIPTOR_RETRIES {
            // First, get just the config descriptor to find total length
            let mut buf = [0u8; 9];
            let setup = SetupPacket::get_descriptor(desc_type::CONFIGURATION, index, 9);
            let short_length = setup.length;
            let mut short_result = Err(UsbError::Timeout(
                "config descriptor short fetch not attempted",
            ));
            let mut short_transferred = 0usize;
            for attempt in 1..=CONFIG_DESCRIPTOR_RETRIES {
                info!(
                    "CFG9 slot={} cycle={} attempt={} len={} index={}",
                    self.slot_id, cycle, attempt, short_length, index
                );
                match self.control_transfer(&setup, Some(&mut buf)) {
                    Ok(transferred) => {
                        warn!(
                            "CFG9XFER slot={} transferred={} bytes=[{:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}]",
                            self.slot_id,
                            transferred,
                            buf[0],
                            buf[1],
                            buf[2],
                            buf[3],
                            buf[4],
                            buf[5],
                            buf[6],
                            buf[7],
                            buf[8]
                        );
                        short_transferred = transferred;
                        short_result = Ok(());
                        break;
                    }
                    Err(UsbError::Stall) => {
                        warn!(
                            "CFG9STALL slot={} cycle={} attempt={}",
                            self.slot_id, cycle, attempt
                        );
                        self.recover_ep0_stall();
                        self.short_settle();
                        short_result = Err(UsbError::Stall);
                    }
                    Err(err) => {
                        short_result = Err(err);
                        break;
                    }
                }
            }
            short_result?;
            if short_transferred < core::mem::size_of::<ConfigDesc>() {
                warn!(
                    "CFG9SHORT slot={} cycle={} transferred={} expected={} bytes=[{:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}]",
                    self.slot_id,
                    cycle,
                    short_transferred,
                    core::mem::size_of::<ConfigDesc>(),
                    buf[0],
                    buf[1],
                    buf[2],
                    buf[3],
                    buf[4],
                    buf[5],
                    buf[6],
                    buf[7],
                    buf[8]
                );
                self.short_settle();
                continue;
            }

            let length = buf[0];
            let desc_type = buf[1];
            let total_len = u16::from_le_bytes([buf[2], buf[3]]) as usize;
            let num_interfaces = buf[4];
            let config_value = buf[5];
            info!(
                "CFG9RAW slot={} bytes=[{:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}]",
                self.slot_id,
                buf[0],
                buf[1],
                buf[2],
                buf[3],
                buf[4],
                buf[5],
                buf[6],
                buf[7],
                buf[8]
            );
            if length != core::mem::size_of::<ConfigDesc>() as u8
                || desc_type != desc_type::CONFIGURATION
                || total_len < core::mem::size_of::<ConfigDesc>()
            {
                warn!(
                    "CFG9BAD slot={} cycle={} length={} desc_type={} total_length={} config_value={} interfaces={} bytes=[{:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}]",
                    self.slot_id,
                    cycle,
                    length,
                    desc_type,
                    total_len,
                    config_value,
                    num_interfaces,
                    buf[0],
                    buf[1],
                    buf[2],
                    buf[3],
                    buf[4],
                    buf[5],
                    buf[6],
                    buf[7],
                    buf[8]
                );
                self.short_settle();
                continue;
            }
            info!(
                "CFG9OK slot={} total_length={} config_value={} interfaces={}",
                self.slot_id, total_len, config_value, num_interfaces
            );

            // Now get the full descriptor
            let mut full_buf = alloc::vec![0u8; total_len];
            let setup =
                SetupPacket::get_descriptor(desc_type::CONFIGURATION, index, total_len as u16);
            let full_length = setup.length;
            let mut full_result = Err(UsbError::Timeout(
                "config descriptor full fetch not attempted",
            ));
            let mut full_transferred = 0usize;
            for attempt in 1..=CONFIG_DESCRIPTOR_RETRIES {
                info!(
                    "CFGF slot={} cycle={} attempt={} len={} index={}",
                    self.slot_id, cycle, attempt, full_length, index
                );
                match self.control_transfer(&setup, Some(&mut full_buf)) {
                    Ok(transferred) => {
                        full_transferred = transferred;
                        full_result = Ok(());
                        break;
                    }
                    Err(UsbError::Stall) => {
                        warn!(
                            "CFGFSTALL slot={} cycle={} attempt={}",
                            self.slot_id, cycle, attempt
                        );
                        self.recover_ep0_stall();
                        self.short_settle();
                        full_result = Err(UsbError::Stall);
                    }
                    Err(err) => {
                        full_result = Err(err);
                        break;
                    }
                }
            }
            full_result?;
            if full_transferred < total_len {
                warn!(
                    "CFGFSHORT slot={} cycle={} transferred={} expected={} head=[{:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}]",
                    self.slot_id,
                    cycle,
                    full_transferred,
                    total_len,
                    full_buf.get(0).copied().unwrap_or(0),
                    full_buf.get(1).copied().unwrap_or(0),
                    full_buf.get(2).copied().unwrap_or(0),
                    full_buf.get(3).copied().unwrap_or(0),
                    full_buf.get(4).copied().unwrap_or(0),
                    full_buf.get(5).copied().unwrap_or(0),
                    full_buf.get(6).copied().unwrap_or(0),
                    full_buf.get(7).copied().unwrap_or(0),
                    full_buf.get(8).copied().unwrap_or(0),
                    full_buf.get(9).copied().unwrap_or(0),
                    full_buf.get(10).copied().unwrap_or(0),
                    full_buf.get(11).copied().unwrap_or(0),
                    full_buf.get(12).copied().unwrap_or(0),
                    full_buf.get(13).copied().unwrap_or(0),
                    full_buf.get(14).copied().unwrap_or(0),
                    full_buf.get(15).copied().unwrap_or(0)
                );
                self.short_settle();
                continue;
            }

            return Ok(full_buf);
        }

        Err(UsbError::InvalidDescriptor)
    }

    /// Set configuration
    pub fn set_configuration(&self, config: u8) -> Result<()> {
        let setup = SetupPacket::set_configuration(config);
        self.control_transfer(&setup, None)?;
        Ok(())
    }

    /// Configure an endpoint (after SET_CONFIGURATION)
    pub fn configure_endpoint(&self, ep: &EndpointDesc) -> Result<()> {
        let host = self.ctrl.host();

        let ep_num = ep.number();
        let is_in = ep.is_in();
        let ep_type = ep.transfer_type();

        // Endpoint Context Index: EP1 OUT = 2, EP1 IN = 3, EP2 OUT = 4, etc.
        let dci = (ep_num as usize * 2) + if is_in { 1 } else { 0 };
        let ring_idx = dci - 1; // rings array is 0-indexed for EP1+

        // Allocate transfer ring for this endpoint
        let ring = Ring::new(host, 256)?;
        let ring_phys = ring.phys(host);

        // Update input context
        let input = self.input_ctx.as_ptr::<InputContext>();
        unsafe {
            (*input).input_control[0] = 0; // Drop flags
            (*input).input_control[1] = (1 << dci) | 1; // Add flags: this EP + Slot

            // The slot context must advertise the highest valid endpoint context
            // index before CONFIGURE_ENDPOINT, or xHCI reports Parameter Error.
            let max_context_entries = (*input).slot.dw0 >> 27;
            if max_context_entries < dci as u32 {
                (*input).slot.dw0 &= !(0x1f << 27);
                (*input).slot.dw0 |= (dci as u32) << 27;
            }

            // xHCI endpoint type encoding
            let xhci_ep_type = match (ep_type, is_in) {
                (0, _) => 4,     // Control (bidirectional)
                (1, false) => 1, // Isoch OUT
                (1, true) => 5,  // Isoch IN
                (2, false) => 2, // Bulk OUT
                (2, true) => 6,  // Bulk IN
                (3, false) => 3, // Interrupt OUT
                (3, true) => 7,  // Interrupt IN
                _ => 4,
            };

            // Calculate interval for xHCI (different from USB descriptor)
            let interval = if self.speed >= reg::SPEED_HIGH {
                ep.interval.saturating_sub(1)
            } else {
                // For FS/LS, convert ms to 125us frames
                // Use integer log2: find highest set bit
                let ms = ep.interval.max(1) as u32;
                let log2_ceil = if ms.is_power_of_two() {
                    ms.trailing_zeros() as u8
                } else {
                    (u32::BITS - ms.leading_zeros()) as u8
                };
                log2_ceil + 3
            };

            (*input).endpoints[ring_idx] =
                EndpointContext::new(xhci_ep_type, ep.max_packet_size, 0, interval, ring_phys);
        }

        // Store ring
        let mut ep_rings = self.ep_rings.lock();
        ep_rings[ring_idx] = Some(ring);
        drop(ep_rings);

        // Configure Endpoint command
        let trb = Trb {
            param: self.input_ctx.phys(host),
            status: 0,
            control: (trb_type::CONFIGURE_ENDPOINT << 10) | ((self.slot_id as u32) << 24),
        };
        self.ctrl.submit_command(trb)?;

        Ok(())
    }

    /// Queue a transfer on an endpoint
    pub fn queue_transfer(
        &self,
        ep_num: u8,
        is_in: bool,
        buf: &PhysMem<H>,
        len: usize,
    ) -> Result<()> {
        let dci = (ep_num as usize * 2) + if is_in { 1 } else { 0 };
        let ring_idx = dci - 1;

        let mut ep_rings = self.ep_rings.lock();
        let ring = ep_rings[ring_idx].as_mut().ok_or(UsbError::InvEndpoint)?;

        let host = self.ctrl.host();
        if len == 0 {
            let trb = Trb {
                param: buf.phys(host),
                status: 0,
                control: (trb_type::NORMAL << 10) | (1 << 5), // IOC
            };
            ring.enqueue(host, trb);
        } else {
            let mut remaining = len;
            let mut offset = 0;
            while remaining > 0 {
                let phys_addr = buf.phys(host) + offset as u64;
                // Calculate bytes remaining to the next 64KB physical boundary
                let next_64k_boundary = (phys_addr + 65536) & !65535u64;
                let max_chunk_to_boundary = (next_64k_boundary - phys_addr) as usize;

                let chunk = remaining.min(65536).min(max_chunk_to_boundary);
                remaining -= chunk;
                let is_last = remaining == 0;
                let remaining_packets = (remaining + 511) / 512;
                let td_size = remaining_packets.min(31) as u32;
                let control = if is_last {
                    (trb_type::NORMAL << 10) | (1 << 5) // IOC
                } else {
                    (trb_type::NORMAL << 10) | (1 << 4) | (1 << 2) // Chain bit + ISP (Interrupt on Short Packet)
                };
                let trb = Trb {
                    param: phys_addr,
                    status: (chunk as u32) | (td_size << 17),
                    control,
                };
                ring.enqueue(host, trb);
                offset += chunk;
            }
        }
        drop(ep_rings);

        // Ring doorbell
        self.ctrl.ring_doorbell(self.slot_id, dci as u8);

        Ok(())
    }

    fn wait_transfer_event(&self, endpoint_id: u8, requested_len: usize) -> Result<usize> {
        let mut loop_counter = 0usize;
        loop {
            loop_counter += 1;
            if loop_counter > 250_000 {
                return Err(UsbError::Timeout(
                    "bulk transfer waiting for TRANSFER_EVENT",
                ));
            }

            if let Some(evt) = self.ctrl.poll_event_matching(|evt| {
                evt.trb_type() == trb_type::TRANSFER_EVENT as u8
                    && evt.slot_id() == self.slot_id
                    && evt.endpoint_id() == endpoint_id
            }) {
                let code = evt.completion_code();
                return match code {
                    completion::SUCCESS | completion::SHORT_PACKET => {
                        Ok(requested_len.saturating_sub(evt.transfer_length() as usize))
                    }
                    completion::INVALID => continue,
                    completion::STALL_ERROR => Err(UsbError::Stall),
                    _ => Err(UsbError::XferFail(code)),
                };
            }

            spin_loop();
        }
    }

    /// Submit a bulk/interrupt transfer and wait for its completion.
    pub fn bulk_transfer(
        &self,
        ep_num: u8,
        is_in: bool,
        buf: &PhysMem<H>,
        len: usize,
    ) -> Result<usize> {
        self.queue_transfer(ep_num, is_in, buf, len)?;

        let dci = (ep_num * 2) + if is_in { 1 } else { 0 };
        self.wait_transfer_event(dci, len)
    }

    /// Returns the xHCI slot ID assigned to this device.
    pub fn slot_id(&self) -> u8 {
        self.slot_id
    }

    /// Returns the root hub port number this device is connected to.
    pub fn port(&self) -> u8 {
        self.port
    }

    /// Returns the device speed (see `reg::SPEED_*` constants).
    pub fn speed(&self) -> u8 {
        self.speed
    }

    /// Returns a reference to the xHCI controller.
    pub fn ctrl(&self) -> &Arc<XhciCtrl<H>> {
        &self.ctrl
    }
}

impl<H: Dma> Drop for UsbDevice<H> {
    fn drop(&mut self) {
        let _ = self.ctrl.disable_slot(self.slot_id);

        let host = self.ctrl.host();

        // Free endpoint rings
        let mut ep_rings = self.ep_rings.lock();
        for ring in ep_rings.iter_mut() {
            if let Some(r) = ring.take() {
                r.free(host);
            }
        }
        drop(ep_rings);

        // Free EP0 ring
        let ep0_ring = core::mem::replace(&mut *self.ep0_ring.lock(), Ring::new(host, 1).unwrap());
        ep0_ring.free(host);
    }
}
