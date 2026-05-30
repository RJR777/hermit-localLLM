//! Minimal RTL8152/RTL8153 USB Ethernet transport support.

use crate::{
    Dma, Result, UsbError,
    desc::{EndpointDesc, SetupPacket, ep_type},
    dev::UsbDevice,
    ring::PhysMem,
};

use alloc::sync::Arc;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, Ordering};

const RTL8152_REQT_READ: u8 = 0xc0;
const RTL8152_REQT_WRITE: u8 = 0x40;
const RTL8152_REQ_GET_REGS: u8 = 0x05;
const RTL8152_REQ_SET_REGS: u8 = 0x05;

const BYTE_EN_DWORD: u16 = 0xff;
const BYTE_EN_WORD: u16 = 0x33;
const BYTE_EN_BYTE: u16 = 0x11;
const MCU_TYPE_USB: u16 = 0x0000;
const MCU_TYPE_PLA: u16 = 0x0100;

const PLA_IDR: u16 = 0xc000;
const PLA_RCR: u16 = 0xc010;
const PLA_CR: u16 = 0xe813;
const PLA_CR_ALIGNED: u16 = PLA_CR & !0x3;

const RCR_AAP: u32 = 0x0000_0001;
const RCR_APM: u32 = 0x0000_0002;
const RCR_AM: u32 = 0x0000_0004;
const RCR_AB: u32 = 0x0000_0008;

const USB_USB_CTRL: u16 = 0xd406;
const RX_AGG_DISABLE: u16 = 0x0010;
const RX_ZERO_EN: u16 = 0x0080;

const CR_RE: u8 = 0x08;
const CR_TE: u8 = 0x04;
const CR_READBACK_SPINS: usize = 100_000;

const RX_DESC_SIZE: usize = 24;
const TX_DESC_SIZE: usize = 8;
const CRC_SIZE: usize = 4;
const RTL8152_AGG_BUF_SZ: usize = 2048;

const RX_LEN_MASK: u32 = 0x7fff;
const TX_FS: u32 = 1 << 31;
const TX_LS: u32 = 1 << 30;
const TX_LEN_MAX: usize = 0x3ffff;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct RxDesc {
    opts1: u32,
    opts2: u32,
    opts3: u32,
    opts4: u32,
    opts5: u32,
    opts6: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct TxDesc {
    opts1: u32,
    opts2: u32,
}

/// Minimal Realtek RTL8152/RTL8153 USB Ethernet transport.
pub struct Rtl8152Device<H: Dma> {
    device: Arc<UsbDevice<H>>,
    ep_in: u8,
    ep_out: u8,
    mac: [u8; 6],
    first_out_submit_logged: AtomicBool,
    first_out_complete_logged: AtomicBool,
    first_in_submit_logged: AtomicBool,
    first_in_complete_logged: AtomicBool,
}

impl<H: Dma> Rtl8152Device<H> {
    /// Configure the adapter's bulk endpoints and enable RX/TX.
    pub fn from_endpoints(device: Arc<UsbDevice<H>>, endpoints: &[EndpointDesc]) -> Result<Self> {
        let ep_in = endpoints
            .iter()
            .find(|ep| ep.transfer_type() == ep_type::BULK && ep.is_in())
            .copied()
            .ok_or(UsbError::DeviceNotFound)?;
        let ep_out = endpoints
            .iter()
            .find(|ep| ep.transfer_type() == ep_type::BULK && !ep.is_in())
            .copied()
            .ok_or(UsbError::DeviceNotFound)?;

        device.configure_endpoint(&ep_in)?;
        device.configure_endpoint(&ep_out)?;

        let mut rtl = Self {
            device,
            ep_in: ep_in.number(),
            ep_out: ep_out.number(),
            mac: [0; 6],
            first_out_submit_logged: AtomicBool::new(false),
            first_out_complete_logged: AtomicBool::new(false),
            first_in_submit_logged: AtomicBool::new(false),
            first_in_complete_logged: AtomicBool::new(false),
        };

        rtl.initialize_adapter()?;

        info!(
            "RTL8153: bulk endpoints configured (IN ep{} OUT ep{})",
            rtl.ep_in, rtl.ep_out
        );

        Ok(rtl)
    }

    /// Returns the device MAC address.
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Returns the underlying USB device handle.
    pub fn usb_device(&self) -> &Arc<UsbDevice<H>> {
        &self.device
    }

    /// Returns the bulk IN endpoint number.
    pub fn rx_endpoint(&self) -> u8 {
        self.ep_in
    }

    /// Returns the bulk OUT endpoint number.
    pub fn tx_endpoint(&self) -> u8 {
        self.ep_out
    }

    /// Run the adapter bring-up sequence in explicit stages.
    fn initialize_adapter(&mut self) -> Result<()> {
        self.mac = self.read_mac_address()?;
        self.reset_device_state()?;
        self.configure_power_and_phy()?;
        self.configure_rx_tx_path()?;
        self.enable_rx_tx()
    }

    /// Reset or quiesce device state before programming datapath settings.
    fn reset_device_state(&self) -> Result<()> {
        info!(
            "RTL8153: reset_device_state PLA_CR={:#04x}",
            self.read_byte(MCU_TYPE_PLA, PLA_CR)?,
        );
        Ok(())
    }

    /// Program vendor-specific power, PHY, and link defaults.
    fn configure_power_and_phy(&self) -> Result<()> {
        // Placeholder for PHY/power/autoneg configuration steps.
        info!(
            "RTL8153: link/phy polling placeholder (slot={} port={} ep_in={} ep_out={})",
            self.device.slot_id(),
            self.device.port(),
            self.ep_in,
            self.ep_out
        );
        Ok(())
    }

    /// Program datapath options before enabling packet flow.
    fn configure_rx_tx_path(&self) -> Result<()> {
        let rcr = self.read_dword(MCU_TYPE_PLA, PLA_RCR)?;
        let updated_rcr = rcr | RCR_APM | RCR_AM | RCR_AB | RCR_AAP;
        self.write_dword(MCU_TYPE_PLA, PLA_RCR, updated_rcr)?;

        let usb_ctrl = self.read_word(MCU_TYPE_USB, USB_USB_CTRL)?;
        let updated_usb_ctrl = usb_ctrl & !(RX_AGG_DISABLE | RX_ZERO_EN);
        self.write_word(MCU_TYPE_USB, USB_USB_CTRL, updated_usb_ctrl)?;

        info!(
            "RTL8153: configure_rx_tx_path PLA_RCR old={:#010x} new={:#010x} USB_USB_CTRL old={:#06x} new={:#06x}",
            rcr, updated_rcr, usb_ctrl, updated_usb_ctrl
        );
        Ok(())
    }

    /// Enable the adapter's RX/TX datapath.
    pub fn enable_rx_tx(&self) -> Result<()> {
        let before_aligned = self.read_dword(MCU_TYPE_PLA, PLA_CR_ALIGNED)?;
        let cr = self.read_byte(MCU_TYPE_PLA, PLA_CR)?;
        let updated = cr | CR_RE | CR_TE;
        info!(
            "RTL8153: enable_rx_tx PLA_CR old={:#04x} new={:#04x} aligned_before={:#010x}",
            cr, updated, before_aligned
        );
        self.write_byte(MCU_TYPE_PLA, PLA_CR, updated)?;
        let after_aligned = self.read_dword(MCU_TYPE_PLA, PLA_CR_ALIGNED)?;
        let readback = self.read_byte(MCU_TYPE_PLA, PLA_CR)?;
        info!(
            "RTL8153: enable_rx_tx PLA_CR readback={:#04x} requested={:#04x} aligned_after={:#010x}",
            readback, updated, after_aligned
        );
        for _ in 0..CR_READBACK_SPINS {
            spin_loop();
        }
        let delayed_aligned = self.read_dword(MCU_TYPE_PLA, PLA_CR_ALIGNED)?;
        let delayed_readback = self.read_byte(MCU_TYPE_PLA, PLA_CR)?;
        info!(
            "RTL8153: enable_rx_tx PLA_CR delayed_readback={:#04x} requested={:#04x} aligned_delayed={:#010x}",
            delayed_readback, updated, delayed_aligned
        );
        if delayed_readback & CR_TE == 0 {
            return Err(UsbError::XferFail(delayed_readback));
        }
        if delayed_readback & CR_RE == 0 {
            warn!(
                "RTL8153: TX enabled but RX did not stick yet (requested={:#04x}, readback={:#04x}); continuing so bulk endpoint probing can proceed",
                updated, delayed_readback
            );
        }
        Ok(())
    }

    /// Transmit one raw Ethernet frame.
    pub fn write_packet(&self, packet: &[u8]) -> Result<usize> {
        if packet.len() > TX_LEN_MAX {
            return Err(UsbError::NotSupported);
        }

        let host = self.device.ctrl().host();
        let len = TX_DESC_SIZE + packet.len();
        let buf = PhysMem::alloc(host, len, 8)?;

        unsafe {
            let tx_desc = buf.as_ptr::<TxDesc>();
            (*tx_desc).opts1 = (TX_FS | TX_LS | packet.len() as u32).to_le();
            (*tx_desc).opts2 = 0;
            core::ptr::copy_nonoverlapping(
                packet.as_ptr(),
                buf.as_ptr::<u8>().add(TX_DESC_SIZE),
                packet.len(),
            );
        }

        self.log_first_bulk_submit(false, len, packet.len());
        let transferred = self.device.bulk_transfer(self.ep_out, false, &buf, len)?;
        self.log_first_bulk_completion(false, transferred);
        buf.free(host);

        Ok(transferred.saturating_sub(TX_DESC_SIZE))
    }

    /// Receive one raw Ethernet frame into `packet`.
    pub fn read_packet(&self, packet: &mut [u8]) -> Result<usize> {
        let host = self.device.ctrl().host();
        let rx_buf_len = packet.len().max(1536) + RX_DESC_SIZE;
        let buf = PhysMem::alloc(host, rx_buf_len, 8)?;

        self.log_first_bulk_submit(true, rx_buf_len, packet.len());
        let transferred = self
            .device
            .bulk_transfer(self.ep_in, true, &buf, rx_buf_len)?;
        let payload_len = self.copy_received_packet(&buf, transferred, packet)?;
        self.log_first_bulk_completion(true, payload_len);

        buf.free(host);
        Ok(payload_len)
    }

    /// Submit a persistent RX transfer.
    pub fn queue_rx_packet(&self, buf: &PhysMem<H>, len: usize) -> Result<()> {
        self.log_first_bulk_submit(true, len, len.saturating_sub(RX_DESC_SIZE));
        self.device.queue_transfer(self.ep_in, true, buf, len)
    }

    /// Poll for completion of a previously queued RX transfer.
    pub fn poll_rx_packet(
        &self,
        buf: &PhysMem<H>,
        requested_len: usize,
        packet: &mut [u8],
    ) -> Option<Result<usize>> {
        let endpoint_id = (self.ep_in * 2) + 1;
        let Some(evt) = self.device.ctrl().poll_event_matching(|evt| {
            evt.slot_id() == self.device.slot_id()
                && evt.endpoint_id() == endpoint_id
                && evt.trb_type() == crate::trb_type::TRANSFER_EVENT as u8
        }) else {
            return None;
        };

        let result = match evt.completion_code() {
            crate::completion::SUCCESS | crate::completion::SHORT_PACKET => {
                let transferred = requested_len.saturating_sub(evt.transfer_length() as usize);
                let copied = self.copy_received_packet(buf, transferred, packet);
                if let Ok(payload_len) = copied {
                    self.log_first_bulk_completion(true, payload_len);
                }
                copied
            }
            crate::completion::STALL_ERROR => Err(UsbError::Stall),
            code => Err(UsbError::XferFail(code)),
        };

        Some(result)
    }

    /// Returns the size of a suitable RX buffer for a given MTU.
    pub fn rx_buffer_len(payload_len: usize) -> usize {
        (payload_len.max(1536) + RX_DESC_SIZE + CRC_SIZE).max(RTL8152_AGG_BUF_SZ)
    }

    fn copy_received_packet(
        &self,
        buf: &PhysMem<H>,
        transferred: usize,
        packet: &mut [u8],
    ) -> Result<usize> {
        if transferred < RX_DESC_SIZE {
            return Err(UsbError::InvalidDescriptor);
        }

        let frame_len = unsafe {
            let desc = &*buf.as_ptr::<RxDesc>();
            (u32::from_le(desc.opts1) & RX_LEN_MASK) as usize
        };
        let payload_len = frame_len
            .saturating_sub(CRC_SIZE)
            .min(transferred.saturating_sub(RX_DESC_SIZE))
            .min(packet.len());

        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr::<u8>().add(RX_DESC_SIZE),
                packet.as_mut_ptr(),
                payload_len,
            );
        }

        Ok(payload_len)
    }

    fn read_mac_address(&self) -> Result<[u8; 6]> {
        let mut buf = [0u8; 8];
        self.read_reg_block(MCU_TYPE_PLA, PLA_IDR, &mut buf)?;

        let mut mac = [0u8; 6];
        mac.copy_from_slice(&buf[..6]);
        info!(
            "RTL8153: read_mac_address {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        Ok(mac)
    }

    fn log_first_bulk_submit(&self, is_in: bool, usb_len: usize, payload_len: usize) {
        let flag = if is_in {
            &self.first_in_submit_logged
        } else {
            &self.first_out_submit_logged
        };
        if flag
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            info!(
                "RTL8153: first bulk {} submit ep{} usb_len={} payload_len={}",
                if is_in { "IN" } else { "OUT" },
                if is_in { self.ep_in } else { self.ep_out },
                usb_len,
                payload_len
            );
        }
    }

    fn log_first_bulk_completion(&self, is_in: bool, payload_len: usize) {
        let flag = if is_in {
            &self.first_in_complete_logged
        } else {
            &self.first_out_complete_logged
        };
        if flag
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            info!(
                "RTL8153: first bulk {} completion ep{} payload_len={}",
                if is_in { "IN" } else { "OUT" },
                if is_in { self.ep_in } else { self.ep_out },
                payload_len
            );
        }
    }

    fn read_reg_block(&self, mcu_type: u16, reg: u16, data: &mut [u8]) -> Result<()> {
        let setup = SetupPacket::new(
            RTL8152_REQT_READ,
            RTL8152_REQ_GET_REGS,
            reg,
            mcu_type,
            data.len() as u16,
        );
        self.device.control_transfer(&setup, Some(data))?;
        Ok(())
    }

    fn write_reg_block(
        &self,
        mcu_type: u16,
        reg: u16,
        byte_en: u16,
        data: &mut [u8],
    ) -> Result<()> {
        let setup = SetupPacket::new(
            RTL8152_REQT_WRITE,
            RTL8152_REQ_SET_REGS,
            reg,
            mcu_type | byte_en,
            data.len() as u16,
        );
        self.device.control_transfer(&setup, Some(data))?;
        Ok(())
    }

    fn read_dword(&self, mcu_type: u16, reg: u16) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.read_reg_block(mcu_type, reg, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn read_word(&self, mcu_type: u16, reg: u16) -> Result<u16> {
        let aligned = reg & !0x3;
        let shift = ((reg & 0x2) * 8) as u32;
        Ok(((self.read_dword(mcu_type, aligned)? >> shift) & 0xffff) as u16)
    }

    fn write_dword(&self, mcu_type: u16, reg: u16, value: u32) -> Result<()> {
        let mut buf = value.to_le_bytes();
        self.write_reg_block(mcu_type, reg, BYTE_EN_DWORD, &mut buf)
    }

    fn write_word(&self, mcu_type: u16, reg: u16, value: u16) -> Result<()> {
        let aligned = reg & !0x3;
        let shift = ((reg & 0x2) * 8) as u32;
        let byte_en = BYTE_EN_WORD << (reg & 0x2);
        let mut buf = ((value as u32) << shift).to_le_bytes();
        self.write_reg_block(mcu_type, aligned, byte_en, &mut buf)
    }

    fn read_byte(&self, mcu_type: u16, reg: u16) -> Result<u8> {
        let aligned = reg & !0x3;
        let shift = ((reg & 0x3) * 8) as u32;
        Ok(((self.read_dword(mcu_type, aligned)? >> shift) & 0xff) as u8)
    }

    fn write_byte(&self, mcu_type: u16, reg: u16, value: u8) -> Result<()> {
        let aligned = reg & !0x3;
        let shift = ((reg & 0x3) * 8) as u32;
        let byte_en = BYTE_EN_BYTE << (reg & 0x3);
        let mut buf = ((value as u32) << shift).to_le_bytes();
        self.write_reg_block(mcu_type, aligned, byte_en, &mut buf)
    }
}
