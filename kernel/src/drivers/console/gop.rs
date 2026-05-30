use core::ptr;

use embedded_io::{ErrorType, Write};
use hermit_sync::{InterruptTicketMutex, Lazy};

use crate::env::{FramebufferFormat, FramebufferInfo};
use crate::errno::Errno;
use crate::framebuffer_font::FONT;

const GLYPH_WIDTH: usize = 8;
const FONT_HEIGHT: usize = 16;
const FONT_SCALE_X: usize = 2;
const FONT_SCALE_Y: usize = 2;
const CELL_WIDTH: usize = GLYPH_WIDTH * FONT_SCALE_X + 1;
const CELL_HEIGHT: usize = FONT_HEIGHT * FONT_SCALE_Y;
const FG_COLOR: u32 = 0x00ffff00;
const BG_COLOR: u32 = 0x00000000;
const PAGE_WRAP_PAUSE_US: u64 = 0_000_000;

static GOP_CONSOLE: Lazy<InterruptTicketMutex<Option<GopConsole>>> =
	Lazy::new(|| InterruptTicketMutex::new(GopConsole::new()));

pub struct GopConsole {
	info: FramebufferInfo,
	cursor_col: usize,
	cursor_row: usize,
	cols: usize,
	rows: usize,
}

fn encode_pixel(format: FramebufferFormat, color: u32) -> Option<u32> {
	let r = (color >> 16) & 0xff;
	let g = (color >> 8) & 0xff;
	let b = color & 0xff;
	match format {
		FramebufferFormat::Rgb => Some((b << 16) | (g << 8) | r),
		FramebufferFormat::Bgr => Some(color),
		FramebufferFormat::Unknown => None,
	}
}

impl GopConsole {
	pub fn new() -> Option<Self> {
		let info = crate::env::framebuffer_info()?;
		if info.width < CELL_WIDTH || info.height < CELL_HEIGHT {
			return None;
		}
		if !matches!(info.format, FramebufferFormat::Rgb | FramebufferFormat::Bgr) {
			return None;
		}

		let mut console = Self {
			cols: info.width / CELL_WIDTH,
			rows: info.height / CELL_HEIGHT,
			info,
			cursor_col: 0,
			cursor_row: 0,
		};
		console.clear_screen();
		Some(console)
	}

	fn clear_screen(&mut self) {
		for y in 0..self.info.height {
			for x in 0..self.info.width {
				self.write_pixel(x, y, BG_COLOR);
			}
		}
	}

	fn write_pixel(&mut self, x: usize, y: usize, color: u32) {
		let Some(encoded) = encode_pixel(self.info.format, color) else {
			return;
		};

		unsafe {
			ptr::with_exposed_provenance_mut::<u32>(self.info.address)
				.add(y * self.info.stride + x)
				.write_volatile(encoded);
		}
	}

	fn clear_cell(&mut self, col: usize, row: usize) {
		let x0 = col * CELL_WIDTH;
		let y0 = row * CELL_HEIGHT;
		for y in 0..CELL_HEIGHT {
			for x in 0..CELL_WIDTH {
				self.write_pixel(x0 + x, y0 + y, BG_COLOR);
			}
		}
	}

	fn clear_row(&mut self, row: usize) {
		for col in 0..self.cols {
			self.clear_cell(col, row);
		}
	}

	fn scroll(&mut self) {
		crate::arch::processor::udelay(PAGE_WRAP_PAUSE_US);
		self.clear_screen();
		self.cursor_row = 0;
	}

	fn newline(&mut self) {
		self.cursor_col = 0;
		self.cursor_row += 1;
		if self.cursor_row >= self.rows {
			self.scroll();
		}
	}

	fn backspace(&mut self) {
		if self.cursor_col > 0 {
			self.cursor_col -= 1;
			self.clear_cell(self.cursor_col, self.cursor_row);
		}
	}

	fn draw_glyph(&mut self, byte: u8) {
		if self.cursor_col >= self.cols {
			self.newline();
		}

		let glyph_index = byte.saturating_sub(b' ') as usize;
		let glyph_index = glyph_index.min((FONT.len() / FONT_HEIGHT).saturating_sub(1));
		let glyph = &FONT[glyph_index * FONT_HEIGHT..(glyph_index + 1) * FONT_HEIGHT];
		let x0 = self.cursor_col * CELL_WIDTH;
		let y0 = self.cursor_row * CELL_HEIGHT;

		for (row, bits) in glyph.iter().copied().enumerate() {
			for col in 0..GLYPH_WIDTH {
				let mask = 1u8 << (7 - col);
				let color = if bits & mask != 0 { FG_COLOR } else { BG_COLOR };
				let pixel_x = x0 + col * FONT_SCALE_X;
				let pixel_y = y0 + row * FONT_SCALE_Y;
				for dy in 0..FONT_SCALE_Y {
					for dx in 0..FONT_SCALE_X {
						self.write_pixel(pixel_x + dx, pixel_y + dy, color);
					}
				}
			}
		}

		self.cursor_col += 1;
	}

	fn write_byte(&mut self, byte: u8) {
		match byte {
			0x0c => {
				self.clear_screen();
				self.cursor_col = 0;
				self.cursor_row = 0;
			}
			b'\n' => self.newline(),
			b'\r' => self.cursor_col = 0,
			0x08 | 0x7f => self.backspace(),
			b'\t' => {
				for _ in 0..4 {
					self.draw_glyph(b' ');
				}
			}
			0x20..=0x7e => self.draw_glyph(byte),
			_ => self.draw_glyph(b'?'),
		}
	}
}

impl ErrorType for GopConsole {
	type Error = Errno;
}

impl Write for GopConsole {
	fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
		for &byte in buf {
			self.write_byte(byte);
		}
		Ok(buf.len())
	}

	fn flush(&mut self) -> Result<(), Self::Error> {
		Ok(())
	}
}

pub fn write_bytes(buf: &[u8]) {
	let mut guard = GOP_CONSOLE.lock();
	if let Some(console) = guard.as_mut() {
		let _ = console.write(buf);
	}
}

pub fn kernel_gop_marker(index: u32, color: u32) {
	if let Some(fb) = crate::env::framebuffer_info() {
		let width = 32usize;
		let height = 32usize;
		let x0 = (index as usize) * 40;
		let y0 = 0usize;
		let Some(encoded) = encode_pixel(fb.format, color) else {
			return;
		};

		let ptr = ptr::with_exposed_provenance_mut::<u32>(fb.address);
		for row in 0..height {
			for col in 0..width {
				unsafe {
					ptr.add((y0 + row) * fb.stride + x0 + col)
						.write_volatile(encoded);
				}
			}
		}
	}
}
