use hermit_sync::InterruptTicketMutex;

const CONSOLE_LOG_CAPACITY: usize = 256 * 1024;

struct ConsoleLog {
	buffer: [u8; CONSOLE_LOG_CAPACITY],
	start: usize,
	len: usize,
	capture_enabled: bool,
}

impl ConsoleLog {
	const fn new() -> Self {
		Self {
			buffer: [0; CONSOLE_LOG_CAPACITY],
			start: 0,
			len: 0,
			capture_enabled: true,
		}
	}

	fn append(&mut self, mut bytes: &[u8]) {
		if !self.capture_enabled || bytes.is_empty() {
			return;
		}

		if bytes.len() >= CONSOLE_LOG_CAPACITY {
			bytes = &bytes[bytes.len() - CONSOLE_LOG_CAPACITY..];
			self.buffer.copy_from_slice(bytes);
			self.start = 0;
			self.len = CONSOLE_LOG_CAPACITY;
			return;
		}

		let free = CONSOLE_LOG_CAPACITY - self.len;
		if bytes.len() > free {
			let drop_len = bytes.len() - free;
			self.start = (self.start + drop_len) % CONSOLE_LOG_CAPACITY;
			self.len -= drop_len;
		}

		let write_start = (self.start + self.len) % CONSOLE_LOG_CAPACITY;
		let first_len = (CONSOLE_LOG_CAPACITY - write_start).min(bytes.len());
		let second_len = bytes.len() - first_len;

		self.buffer[write_start..write_start + first_len].copy_from_slice(&bytes[..first_len]);
		if second_len > 0 {
			self.buffer[..second_len].copy_from_slice(&bytes[first_len..]);
		}

		self.len += bytes.len();
	}

	fn read(&self, offset: usize, output: &mut [u8]) -> usize {
		if offset >= self.len || output.is_empty() {
			return 0;
		}

		let read_len = (self.len - offset).min(output.len());
		let read_start = (self.start + offset) % CONSOLE_LOG_CAPACITY;
		let first_len = (CONSOLE_LOG_CAPACITY - read_start).min(read_len);
		let second_len = read_len - first_len;

		output[..first_len].copy_from_slice(&self.buffer[read_start..read_start + first_len]);
		if second_len > 0 {
			output[first_len..read_len].copy_from_slice(&self.buffer[..second_len]);
		}

		read_len
	}
}

static CONSOLE_LOG: InterruptTicketMutex<ConsoleLog> = InterruptTicketMutex::new(ConsoleLog::new());

pub(crate) fn append(bytes: &[u8]) {
	CONSOLE_LOG.lock().append(bytes);
}

pub(crate) fn len() -> usize {
	CONSOLE_LOG.lock().len
}

pub(crate) fn read(offset: usize, output: &mut [u8]) -> usize {
	CONSOLE_LOG.lock().read(offset, output)
}

pub(crate) fn set_capture_enabled(enabled: bool) {
	CONSOLE_LOG.lock().capture_enabled = enabled;
}
