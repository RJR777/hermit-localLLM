# usb-oxide

Bare-metal lightweight xHCI/USB stack for OS development.

## Features

- xHCI host controller driver
- USB device enumeration and configuration
- HID class driver (Boot Protocol keyboards & mice)
- Mass Storage class driver (Bulk-Only Transport, SCSI)

## Integration

Implement the `Dma` trait to provide DMA allocation and MMIO mapping:

```rust
impl Dma for MyDma {
    unsafe fn alloc(&self, size: usize, align: usize) -> Option<usize> { /* physically contiguous */ }
    unsafe fn free(&self, addr: usize, size: usize, align: usize) { }
    unsafe fn map_mmio(&self, phys: usize, size: usize) -> Option<usize> { }
    unsafe fn unmap_mmio(&self, virt: usize, size: usize) { }
    fn virt_to_phys(&self, va: usize) -> usize { }
    fn page_size(&self) -> usize { /* typically 4096 */ }
}
```

Then initialise from PCI:

```rust
let ctrl = Arc::new(XhciCtrl::new(pci_bar0_addr, MyDma)?);

for port in 0..ctrl.max_ports() {
    if ctrl.port_connected(port) {
        let dev = UsbDevice::new(ctrl.clone(), port)?;
        // enumerate, configure, use class drivers...
    }
}
```

## Alignment Requirements

The `alloc` function receives alignment requirements per allocation:

| Structure | Align |
|-----------|-------|
| TRB rings | 16 B  |
| Slot/Endpoint contexts | 32 B |
| Device/Input contexts, DCBAA | 64 B |
| Scratchpad buffers | Page |
