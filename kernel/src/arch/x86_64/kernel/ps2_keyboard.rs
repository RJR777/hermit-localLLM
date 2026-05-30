use hermit_sync::InterruptTicketMutex;
use x86_64::instructions::port::Port;

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const COMMAND_PORT: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;

const BUFFER_LEN: usize = 128;
const WAIT_LIMIT: usize = 100_000;
const POLL_LIMIT: usize = 32;

static PS2_KEYBOARD: InterruptTicketMutex<Ps2Keyboard> =
	InterruptTicketMutex::new(Ps2Keyboard::new());

struct Ps2Keyboard {
	present: bool,
	left_shift: bool,
	right_shift: bool,
	ctrl: bool,
	caps_lock: bool,
	extended: bool,
	break_pending: bool,
	scan_set2: bool,
	buffer: [u8; BUFFER_LEN],
	head: usize,
	tail: usize,
}

impl Ps2Keyboard {
	const fn new() -> Self {
		Self {
			present: false,
			left_shift: false,
			right_shift: false,
			ctrl: false,
			caps_lock: false,
			extended: false,
			break_pending: false,
			scan_set2: false,
			buffer: [0; BUFFER_LEN],
			head: 0,
			tail: 0,
		}
	}

	fn available(&self) -> bool {
		self.head != self.tail
	}

	fn push(&mut self, byte: u8) {
		let next = (self.head + 1) % BUFFER_LEN;
		if next != self.tail {
			self.buffer[self.head] = byte;
			self.head = next;
		}
	}

	fn pop(&mut self) -> Option<u8> {
		if self.head == self.tail {
			return None;
		}

		let byte = self.buffer[self.tail];
		self.tail = (self.tail + 1) % BUFFER_LEN;
		Some(byte)
	}

	fn decode(&mut self, scancode: u8) -> Option<u8> {
		if matches!(scancode, 0xfa | 0xfe | 0xaa | 0xee) {
			return None;
		}

		if scancode == 0xe0 {
			self.extended = true;
			return None;
		}

		if scancode == 0xf0 {
			self.break_pending = true;
			self.scan_set2 = true;
			return None;
		}

		if self.scan_set2 {
			return self.decode_set2(scancode);
		}

		if self.extended {
			self.extended = false;
			return match scancode {
				0x1c => Some(b'\n'),
				0xaa => {
					self.left_shift = false;
					None
				}
				0xb6 => {
					self.right_shift = false;
					None
				}
				_ => None,
			};
		}

		let released = (scancode & 0x80) != 0;
		let code = scancode & 0x7f;

		match code {
			0x2a => {
				self.left_shift = !released;
				return None;
			}
			0x36 => {
				self.right_shift = !released;
				return None;
			}
			0x1d => {
				self.ctrl = !released;
				return None;
			}
			0x3a if !released => {
				self.clear_momentary_modifiers();
				self.caps_lock = false;
				return None;
			}
			_ if released => return None,
			_ => {}
		}

		let byte = decode_set1(code, self.shift_active(), self.ctrl, self.caps_lock);
		if byte.is_some() {
			self.clear_momentary_modifiers();
		}
		byte
	}

	fn decode_set2(&mut self, scancode: u8) -> Option<u8> {
		let released = self.break_pending;
		self.break_pending = false;

		if self.extended {
			self.extended = false;
			return match scancode {
				0x5a if !released => Some(b'\n'),
				0x12 => {
					self.left_shift = !released;
					None
				}
				0x59 => {
					self.right_shift = !released;
					None
				}
				0x14 => {
					self.ctrl = !released;
					None
				}
				_ => None,
			};
		}

		match scancode {
			0x12 => {
				self.left_shift = !released;
				return None;
			}
			0x59 => {
				self.right_shift = !released;
				return None;
			}
			0x14 => {
				self.ctrl = !released;
				return None;
			}
			0x58 if !released => {
				self.clear_momentary_modifiers();
				self.caps_lock = false;
				return None;
			}
			_ if released => return None,
			_ => {}
		}

		let byte = decode_set2(scancode, self.shift_active(), self.ctrl, self.caps_lock);
		if byte.is_some() {
			self.clear_momentary_modifiers();
		}
		byte
	}

	fn shift_active(&self) -> bool {
		self.left_shift || self.right_shift
	}

	fn clear_momentary_modifiers(&mut self) {
		self.left_shift = false;
		self.right_shift = false;
	}
}

pub(crate) fn probe_and_enable() {
	if crate::env::is_uhyve() {
		return;
	}

	let initial_status = read_status();
	info!("PS2KBD: probing i8042 status={initial_status:#04x}");

	flush_output();

	let config = read_controller_config();
	match config {
		Some(byte) => info!("PS2KBD: controller config byte={byte:#04x}"),
		None => warn!("PS2KBD: controller config byte unavailable"),
	}

	if !write_command(0xae) {
		warn!("PS2KBD: could not enable first PS/2 port");
		return;
	}

	let set1 = set_keyboard_scan_code_set1();
	info!("PS2KBD: request scan-code set 1 result={set1}");
	{
		let mut keyboard = PS2_KEYBOARD.lock();
		keyboard.scan_set2 = false;
		keyboard.left_shift = false;
		keyboard.right_shift = false;
		keyboard.ctrl = false;
		keyboard.break_pending = false;
		keyboard.extended = false;
	}

	let enabled = write_data(0xf4);
	let response = if enabled { read_response() } else { None };
	info!("PS2KBD: keyboard scan enable sent={enabled} response={response:?}");

	if response == Some(0xfa) {
		PS2_KEYBOARD.lock().present = true;
		info!("PS2KBD: PS/2 keyboard activated; console stdin polling enabled");
	} else {
		warn!(
			"PS2KBD: no PS/2 keyboard ACK; internal keyboard may be I2C-HID or firmware-disabled"
		);
	}
}

pub(crate) fn read_ready() -> bool {
	poll();
	PS2_KEYBOARD.lock().available()
}

pub(crate) fn polling_enabled() -> bool {
	PS2_KEYBOARD.lock().present
}

pub(crate) fn read(buf: &mut [u8]) -> usize {
	poll();

	let mut guard = PS2_KEYBOARD.lock();
	let mut count = 0;
	for slot in buf.iter_mut() {
		if let Some(byte) = guard.pop() {
			*slot = byte;
			count += 1;
		} else {
			break;
		}
	}

	count
}

fn poll() {
	let mut guard = PS2_KEYBOARD.lock();
	if !guard.present {
		return;
	}

	for _ in 0..POLL_LIMIT {
		if (read_status() & STATUS_OUTPUT_FULL) == 0 {
			break;
		}

		let scancode = read_data();
		if let Some(byte) = guard.decode(scancode) {
			guard.push(byte);
		}
	}
}

fn read_controller_config() -> Option<u8> {
	if !write_command(0x20) {
		return None;
	}

	read_response()
}

fn flush_output() {
	for _ in 0..POLL_LIMIT {
		if (read_status() & STATUS_OUTPUT_FULL) == 0 {
			break;
		}
		let _ = read_data();
	}
}

fn read_response() -> Option<u8> {
	if wait_output_full() {
		Some(read_data())
	} else {
		None
	}
}

fn set_keyboard_scan_code_set1() -> bool {
	if !write_data(0xf0) {
		return false;
	}
	if read_response() != Some(0xfa) {
		return false;
	}
	if !write_data(0x01) {
		return false;
	}
	read_response() == Some(0xfa)
}

fn write_command(command: u8) -> bool {
	if !wait_input_clear() {
		return false;
	}

	unsafe {
		Port::<u8>::new(COMMAND_PORT).write(command);
	}
	true
}

fn write_data(data: u8) -> bool {
	if !wait_input_clear() {
		return false;
	}

	unsafe {
		Port::<u8>::new(DATA_PORT).write(data);
	}
	true
}

fn wait_input_clear() -> bool {
	for _ in 0..WAIT_LIMIT {
		if (read_status() & STATUS_INPUT_FULL) == 0 {
			return true;
		}
		core::hint::spin_loop();
	}

	false
}

fn wait_output_full() -> bool {
	for _ in 0..WAIT_LIMIT {
		if (read_status() & STATUS_OUTPUT_FULL) != 0 {
			return true;
		}
		core::hint::spin_loop();
	}

	false
}

fn read_status() -> u8 {
	unsafe { Port::<u8>::new(STATUS_PORT).read() }
}

fn read_data() -> u8 {
	unsafe { Port::<u8>::new(DATA_PORT).read() }
}

fn decode_set1(code: u8, shift: bool, ctrl: bool, caps_lock: bool) -> Option<u8> {
	if ctrl && code == 0x2e {
		return Some(3);
	}

	let byte = match code {
		0x01 => 0x1b,
		0x02 => shifted(b'1', b'!', shift),
		0x03 => shifted(b'2', b'@', shift),
		0x04 => shifted(b'3', b'#', shift),
		0x05 => shifted(b'4', b'$', shift),
		0x06 => shifted(b'5', b'%', shift),
		0x07 => shifted(b'6', b'^', shift),
		0x08 => shifted(b'7', b'&', shift),
		0x09 => shifted(b'8', b'*', shift),
		0x0a => shifted(b'9', b'(', shift),
		0x0b => shifted(b'0', b')', shift),
		0x0c => shifted(b'-', b'_', shift),
		0x0d => shifted(b'=', b'+', shift),
		0x0e => 0x08,
		0x0f => b'\t',
		0x10 => letter(b'q', shift, caps_lock),
		0x11 => letter(b'w', shift, caps_lock),
		0x12 => letter(b'e', shift, caps_lock),
		0x13 => letter(b'r', shift, caps_lock),
		0x14 => letter(b't', shift, caps_lock),
		0x15 => letter(b'y', shift, caps_lock),
		0x16 => letter(b'u', shift, caps_lock),
		0x17 => letter(b'i', shift, caps_lock),
		0x18 => letter(b'o', shift, caps_lock),
		0x19 => letter(b'p', shift, caps_lock),
		0x1a => shifted(b'[', b'{', shift),
		0x1b => shifted(b']', b'}', shift),
		0x1c => b'\n',
		0x1e => letter(b'a', shift, caps_lock),
		0x1f => letter(b's', shift, caps_lock),
		0x20 => letter(b'd', shift, caps_lock),
		0x21 => letter(b'f', shift, caps_lock),
		0x22 => letter(b'g', shift, caps_lock),
		0x23 => letter(b'h', shift, caps_lock),
		0x24 => letter(b'j', shift, caps_lock),
		0x25 => letter(b'k', shift, caps_lock),
		0x26 => letter(b'l', shift, caps_lock),
		0x27 => shifted(b';', b':', shift),
		0x28 => shifted(b'\'', b'"', shift),
		0x29 => shifted(b'`', b'~', shift),
		0x2b => shifted(b'\\', b'|', shift),
		0x2c => letter(b'z', shift, caps_lock),
		0x2d => letter(b'x', shift, caps_lock),
		0x2e => letter(b'c', shift, caps_lock),
		0x2f => letter(b'v', shift, caps_lock),
		0x30 => letter(b'b', shift, caps_lock),
		0x31 => letter(b'n', shift, caps_lock),
		0x32 => letter(b'm', shift, caps_lock),
		0x33 => shifted(b',', b'<', shift),
		0x34 => shifted(b'.', b'>', shift),
		0x35 => shifted(b'/', b'?', shift),
		0x39 => b' ',
		_ => return None,
	};

	Some(byte)
}

fn decode_set2(code: u8, shift: bool, ctrl: bool, caps_lock: bool) -> Option<u8> {
	if ctrl && code == 0x21 {
		return Some(3);
	}

	let byte = match code {
		0x76 => 0x1b,
		0x16 => shifted(b'1', b'!', shift),
		0x1e => shifted(b'2', b'@', shift),
		0x26 => shifted(b'3', b'#', shift),
		0x25 => shifted(b'4', b'$', shift),
		0x2e => shifted(b'5', b'%', shift),
		0x36 => shifted(b'6', b'^', shift),
		0x3d => shifted(b'7', b'&', shift),
		0x3e => shifted(b'8', b'*', shift),
		0x46 => shifted(b'9', b'(', shift),
		0x45 => shifted(b'0', b')', shift),
		0x4e => shifted(b'-', b'_', shift),
		0x55 => shifted(b'=', b'+', shift),
		0x66 => 0x08,
		0x0d => b'\t',
		0x15 => letter(b'q', shift, caps_lock),
		0x1d => letter(b'w', shift, caps_lock),
		0x24 => letter(b'e', shift, caps_lock),
		0x2d => letter(b'r', shift, caps_lock),
		0x2c => letter(b't', shift, caps_lock),
		0x35 => letter(b'y', shift, caps_lock),
		0x3c => letter(b'u', shift, caps_lock),
		0x43 => letter(b'i', shift, caps_lock),
		0x44 => letter(b'o', shift, caps_lock),
		0x4d => letter(b'p', shift, caps_lock),
		0x54 => shifted(b'[', b'{', shift),
		0x5b => shifted(b']', b'}', shift),
		0x5a => b'\n',
		0x1c => letter(b'a', shift, caps_lock),
		0x1b => letter(b's', shift, caps_lock),
		0x23 => letter(b'd', shift, caps_lock),
		0x2b => letter(b'f', shift, caps_lock),
		0x34 => letter(b'g', shift, caps_lock),
		0x33 => letter(b'h', shift, caps_lock),
		0x3b => letter(b'j', shift, caps_lock),
		0x42 => letter(b'k', shift, caps_lock),
		0x4b => letter(b'l', shift, caps_lock),
		0x4c => shifted(b';', b':', shift),
		0x52 => shifted(b'\'', b'"', shift),
		0x0e => shifted(b'`', b'~', shift),
		0x5d => shifted(b'\\', b'|', shift),
		0x1a => letter(b'z', shift, caps_lock),
		0x22 => letter(b'x', shift, caps_lock),
		0x21 => letter(b'c', shift, caps_lock),
		0x2a => letter(b'v', shift, caps_lock),
		0x32 => letter(b'b', shift, caps_lock),
		0x31 => letter(b'n', shift, caps_lock),
		0x3a => letter(b'm', shift, caps_lock),
		0x41 => shifted(b',', b'<', shift),
		0x49 => shifted(b'.', b'>', shift),
		0x4a => shifted(b'/', b'?', shift),
		0x29 => b' ',
		_ => return None,
	};

	Some(byte)
}

fn shifted(normal: u8, shifted: u8, active: bool) -> u8 {
	if active { shifted } else { normal }
}

fn letter(lower: u8, shift: bool, caps_lock: bool) -> u8 {
	if shift ^ caps_lock { lower - 32 } else { lower }
}
