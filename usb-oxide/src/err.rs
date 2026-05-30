//! USB error types.

use core::result::Result as CoreResult;

/// USB driver error types.
#[derive(Debug, Clone, Copy)]
pub enum UsbError {
    /// Operation timed out with a message
    Timeout(&'static str),
    /// Out of memory
    OoRam,
    /// Failed to map MMIO region
    MapFail,
    /// Invalid slot ID
    InvSlot,
    /// Invalid port number
    InvPort,
    /// Invalid endpoint
    InvEndpoint,
    /// Command failed with completion code
    CmdFail(u8),
    /// Transfer failed with completion code
    XferFail(u8),
    /// Device not found
    DeviceNotFound,
    /// Operation not supported
    NotSupported,
    /// Invalid descriptor
    InvalidDescriptor,
    /// Endpoint stalled
    Stall,
    /// Failed to parse descriptor
    ParseError,
}

/// Result type for USB operations.
pub type Result<T> = CoreResult<T, UsbError>;
