use hermit as _;

use std::alloc::{alloc, dealloc, Layout};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use std::vec::Vec;

extern "C" {
    fn sys_get_model_ptr() -> *const u8;
    fn sys_get_model_len() -> usize;
    fn sys_get_timer_ticks() -> u64;
    fn sys_append_memory_db(data: *const u8, data_len: usize) -> i32;
    fn sys_read_memory_db(buf: *mut u8, buf_len: usize) -> i32;
    fn sys_erase_memory_db() -> i32;
    fn sys_console_log_len() -> usize;
    fn sys_read_console_log(offset: usize, buf: *mut u8, buf_len: usize) -> i32;
    fn sys_set_console_log_capture(enabled: i32) -> i32;
    fn sys_reboot() -> !;
    fn hermit_bitnet_prompt_decode_from_buffer(
        model_data: *const core::ffi::c_void,
        model_len: u64,
        prompt: *const i8,
        n_predict: i32,
        n_threads: i32,
        n_ctx: i32,
        output: *mut i8,
        output_len: u64,
    ) -> i32;
}

const BITNET_SHELL_N_PREDICT: i32 = 64;
const BITNET_SHELL_N_CTX: i32 = 512;
const BITNET_RUNTIME_WARMUP_PROMPT: &[u8] = b"runtime warmup\0";
const BITNET_OUTPUT_BUF_LEN: usize = 16 * 1024;
const RAM_SHELL_LINE_LEN: usize = 256;
const RAM_SHELL_HISTORY_CAP_BYTES: usize = 64 * 1024;
const RAM_SHELL_LOG_PAGE_BYTES: usize = 1024;
const RAM_SHELL_COMMAND_PROMPT: &[u8] = b"ram> ";
const RAM_SHELL_MODEL_PROMPT: &[u8] = b"model> ";
const RAM_SHELL_LOG_PROMPT: &[u8] = b"log> ";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RamShellMode {
    Command,
    Prompt,
    Log,
}

struct QaRecord {
    question: Vec<u8>,
    answer: Vec<u8>,
}

static BITAPP_GENERATION_ABORTED: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn bitapp_generation_should_abort() -> i32 {
    let mut stdin = io::stdin();
    let mut byte = [0u8; 1];

    loop {
        match stdin.read(&mut byte) {
            Ok(0) => return 0,
            Ok(_) if byte[0] == 3 => {
                BITAPP_GENERATION_ABORTED.store(true, Ordering::Release);
                return 1;
            }
            Ok(_) => {}
            Err(_) => return 0,
        }
    }
}
#[no_mangle]
pub unsafe extern "C" fn bitapp_malloc(size: usize, align: usize) -> *mut u8 {
    let size = size.max(1);
    let align = align
        .max(core::mem::align_of::<usize>())
        .next_power_of_two();
    match Layout::from_size_align(size, align) {
        Ok(layout) => unsafe { alloc(layout) },
        Err(_) => core::ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn bitapp_free(ptr: *mut u8, size: usize, align: usize) {
    if ptr.is_null() {
        return;
    }
    let size = size.max(1);
    let align = align
        .max(core::mem::align_of::<usize>())
        .next_power_of_two();
    if let Ok(layout) = Layout::from_size_align(size, align) {
        unsafe {
            dealloc(ptr, layout);
        }
    }
}

fn bitapp_n_threads() -> i32 {
    if option_env!("BITAPP_ENABLE_SMP_PTHREAD") != Some("1") {
        return 1;
    }
    option_env!("BITNET_N_THREADS")
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(1)
}

fn c_string_len(buf: &[u8]) -> usize {
    buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len())
}

fn build_raw_prompt(prompt_buf: &mut [u8], prompt: &str) -> usize {
    let prompt_bytes = prompt.as_bytes();
    if prompt_bytes.len() >= prompt_buf.len() {
        return 0;
    }
    prompt_buf[..prompt_bytes.len()].copy_from_slice(prompt_bytes);
    prompt_buf[prompt_bytes.len()] = 0;
    prompt_bytes.len()
}

fn qa_record_payload_len(record: &QaRecord) -> usize {
    record
        .question
        .len()
        .saturating_add(record.answer.len())
        .saturating_add(8)
}

fn history_payload_len(history: &[QaRecord]) -> usize {
    history.iter().map(qa_record_payload_len).sum()
}

fn shrink_records(history: &mut Vec<QaRecord>, needed: usize) {
    while !history.is_empty()
        && history_payload_len(history.as_slice()).saturating_add(needed)
            > RAM_SHELL_HISTORY_CAP_BYTES
    {
        history.remove(0);
    }
}

fn shrink_bytes(buf: &mut Vec<u8>, needed: usize) {
    let required = buf.len().saturating_add(needed);
    if required <= RAM_SHELL_HISTORY_CAP_BYTES {
        return;
    }
    if needed >= RAM_SHELL_HISTORY_CAP_BYTES {
        buf.clear();
        return;
    }
    let drop_len = required.saturating_sub(RAM_SHELL_HISTORY_CAP_BYTES);
    buf.drain(0..drop_len.min(buf.len()));
}

fn append_decimal(target: &mut Vec<u8>, mut value: usize) {
    let mut digits = [0u8; 20];
    let mut len = 0usize;
    if value == 0 {
        target.push(b'0');
        return;
    }
    while value > 0 {
        digits[len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
    }
    while len > 0 {
        len -= 1;
        target.push(digits[len]);
    }
}

fn append_serialized_qa(target: &mut Vec<u8>, prompt: &str, answer: &[u8]) {
    if !target.is_empty() {
        target.push(b'\n');
    }
    target.extend_from_slice(b"[RAMQA v1 q=");
    append_decimal(target, prompt.as_bytes().len());
    target.extend_from_slice(b" a=");
    append_decimal(target, answer.len());
    target.extend_from_slice(b"]\n");
    target.extend_from_slice(prompt.as_bytes());
    target.push(b'\n');
    target.extend_from_slice(answer);
    target.push(b'\n');
}

fn append_serialized_forget(target: &mut Vec<u8>, key: &[u8]) {
    if !target.is_empty() {
        target.push(b'\n');
    }
    target.extend_from_slice(b"[RAMDEL v1 k=");
    append_decimal(target, key.len());
    target.extend_from_slice(b"]\n");
    target.extend_from_slice(key);
    target.push(b'\n');
}

fn append_to_history(
    history: &mut Vec<QaRecord>,
    session_delta: &mut Vec<u8>,
    prompt: &str,
    answer: &[u8],
) {
    let needed = prompt.len().saturating_add(answer.len()).saturating_add(64);
    shrink_records(history, needed);
    shrink_bytes(session_delta, needed);

    upsert_history_record(
        history,
        QaRecord {
            question: prompt.as_bytes().to_vec(),
            answer: answer.to_vec(),
        },
    );
    append_serialized_qa(session_delta, prompt, answer);
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

fn eq_ignore_ascii_case_byte(left: u8, right: u8) -> bool {
    left.to_ascii_lowercase() == right.to_ascii_lowercase()
}

fn contains_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle.iter())
            .all(|(&left, &right)| eq_ignore_ascii_case_byte(left, right))
    })
}

fn eq_ignore_ascii_case_slice(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(&left, &right)| eq_ignore_ascii_case_byte(left, right))
}

fn contains_word_sequence(text: &[u8], words: &[&[u8]]) -> bool {
    if words.is_empty() {
        return false;
    }

    let mut word_index = 0usize;
    let mut start = 0usize;
    while start < text.len() {
        while start < text.len() && !is_word_byte(text[start]) {
            start += 1;
        }
        let mut end = start;
        while end < text.len() && is_word_byte(text[end]) {
            end += 1;
        }

        if start < end && eq_ignore_ascii_case_slice(&text[start..end], words[word_index]) {
            word_index += 1;
            if word_index == words.len() {
                return true;
            }
        } else if start < end {
            word_index = if eq_ignore_ascii_case_slice(&text[start..end], words[0]) {
                1
            } else {
                0
            };
        }

        start = end.saturating_add(1);
    }
    false
}

fn memory_key_from_text(text: &[u8]) -> Option<Vec<u8>> {
    if contains_word_sequence(text, &[b"secure", b"phrase"])
        || contains_word_sequence(text, &[b"stored", b"secure", b"phrase"])
        || contains_word_sequence(text, &[b"secured", b"stored", b"phrase"])
    {
        return Some(b"secure phrase".to_vec());
    }
    if contains_word_sequence(text, &[b"secret", b"phrase"])
        || contains_word_sequence(text, &[b"secret", b"key"])
    {
        return Some(b"secret phrase".to_vec());
    }
    if contains_word_sequence(text, &[b"memory", b"test", b"phrase"]) {
        return Some(b"memory test phrase".to_vec());
    }
    if contains_word_sequence(text, &[b"project", b"codename"])
        || contains_word_sequence(text, &[b"code", b"name"])
        || contains_word_sequence(text, &[b"codename"])
    {
        return Some(b"codename".to_vec());
    }
    None
}

fn starts_with_question_word(text: &[u8]) -> bool {
    let text = trim_ascii(text);
    let mut end = 0usize;
    while end < text.len() && is_word_byte(text[end]) {
        end += 1;
    }
    if end == 0 {
        return false;
    }

    let first = &text[..end];
    [
        b"what" as &[u8],
        b"which",
        b"who",
        b"when",
        b"where",
        b"why",
        b"how",
        b"do",
        b"does",
        b"did",
        b"is",
        b"are",
        b"can",
        b"will",
    ]
    .iter()
    .any(|word| eq_ignore_ascii_case_slice(first, word))
}

fn looks_like_question(text: &[u8]) -> bool {
    text.iter().any(|&byte| byte == b'?') || starts_with_question_word(text)
}

fn qa_record_key(record: &QaRecord) -> Option<Vec<u8>> {
    if !looks_like_question(record.question.as_slice()) {
        if let Some(key) = memory_key_from_text(record.question.as_slice()) {
            return Some(key);
        }
    }
    memory_key_from_text(record.answer.as_slice())
}

fn upsert_history_record(history: &mut Vec<QaRecord>, record: QaRecord) {
    if let Some(key) = qa_record_key(&record) {
        history.retain(|existing| qa_record_key(existing).as_deref() != Some(key.as_slice()));
    }
    history.push(record);
}

fn record_matches_forget_phrase(record: &QaRecord, phrase: &[u8]) -> bool {
    let phrase = trim_ascii(phrase);
    if phrase.is_empty() {
        return false;
    }

    if let Some(key) = memory_key_from_text(phrase) {
        if qa_record_key(record).as_deref() == Some(key.as_slice())
            || record_mentions_key(record, key.as_slice())
        {
            return true;
        }
    }

    contains_case_insensitive(record.question.as_slice(), phrase)
        || contains_case_insensitive(record.answer.as_slice(), phrase)
}

fn forget_history_records(history: &mut Vec<QaRecord>, phrase: &[u8]) -> usize {
    let before = history.len();
    history.retain(|record| !record_matches_forget_phrase(record, phrase));
    before.saturating_sub(history.len())
}

fn append_forget_to_history(
    history: &mut Vec<QaRecord>,
    session_delta: &mut Vec<u8>,
    phrase: &str,
) -> usize {
    let phrase = trim_ascii(phrase.as_bytes());
    if phrase.is_empty() {
        return 0;
    }

    let key = memory_key_from_text(phrase).unwrap_or_else(|| phrase.to_vec());
    let needed = key.len().saturating_add(64);
    shrink_bytes(session_delta, needed);
    let removed = forget_history_records(history, key.as_slice());
    append_serialized_forget(session_delta, key.as_slice());
    removed
}

fn record_mentions_key(record: &QaRecord, key: &[u8]) -> bool {
    memory_key_from_text(record.question.as_slice()).as_deref() == Some(key)
        || memory_key_from_text(record.answer.as_slice()).as_deref() == Some(key)
}

fn bitapp_flush_memory_db(session_delta: &mut Vec<u8>) {
    if session_delta.is_empty() {
        return;
    }

    let ret = unsafe { sys_append_memory_db(session_delta.as_ptr().cast(), session_delta.len()) };
    if ret < 0 {
        println!("bitapp: RAM_SHELL failed to persist session delta to SSD memory DB: {ret}");
        return;
    }

    session_delta.clear();
}

fn bitapp_erase_memory_db(history: &mut Vec<QaRecord>, session_delta: &mut Vec<u8>) {
    history.clear();
    session_delta.clear();

    let ret = unsafe { sys_erase_memory_db() };
    if ret < 0 {
        println!("bitapp: RAM_SHELL failed to erase SSD memory DB: {ret}");
    }
}

fn bitapp_reboot(session_delta: &mut Vec<u8>) -> ! {
    bitapp_flush_memory_db(session_delta);
    unsafe { sys_reboot() }
}

fn bitapp_timer_us() -> u64 {
    unsafe { sys_get_timer_ticks() }
}

fn bitapp_elapsed_ms(start: u64, end: u64) -> u64 {
    end.saturating_sub(start) / 1000
}

fn bitapp_boot_ms(now: u64) -> u64 {
    bitapp_elapsed_ms(0, now)
}

fn bitapp_boot_seconds(now: u64) -> u64 {
    now / 1_000_000
}

fn bitapp_boot_milliseconds(now: u64) -> u64 {
    (now % 1_000_000) / 1000
}

fn bitapp_log_boot_event(event: &str) -> u64 {
    let now = bitapp_timer_us();
    println!(
        "[ {}.{:03} ] bitapp: boot_timer event={} boot_ms={}",
        bitapp_boot_seconds(now),
        bitapp_boot_milliseconds(now),
        event,
        bitapp_boot_ms(now)
    );
    now
}

fn bitapp_log_boot_delta(event: &str, since: u64) -> u64 {
    let now = bitapp_timer_us();
    println!(
        "[ {}.{:03} ] bitapp: boot_timer event={} boot_ms={} delta_ms={}",
        bitapp_boot_seconds(now),
        bitapp_boot_milliseconds(now),
        event,
        bitapp_boot_ms(now),
        bitapp_elapsed_ms(since, now)
    );
    now
}

fn bitapp_console_log_len() -> usize {
    unsafe { sys_console_log_len() }
}

fn bitapp_console_log_set_capture(enabled: bool) {
    let value = if enabled { 1 } else { 0 };
    let _ = unsafe { sys_set_console_log_capture(value) };
}

fn bitapp_read_console_log(offset: usize, output: &mut [u8]) -> usize {
    if output.is_empty() {
        return 0;
    }

    let ret = unsafe { sys_read_console_log(offset, output.as_mut_ptr().cast(), output.len()) };
    if ret <= 0 {
        return 0;
    }

    usize::try_from(ret).unwrap_or(0)
}

fn bitapp_console_log_max_offset(log_len: usize) -> usize {
    log_len.saturating_sub(RAM_SHELL_LOG_PAGE_BYTES)
}

fn bitapp_console_log_clamp_offset(offset: usize, log_len: usize) -> usize {
    offset.min(bitapp_console_log_max_offset(log_len))
}

fn bitapp_console_log_bottom_offset() -> usize {
    bitapp_console_log_max_offset(bitapp_console_log_len())
}

fn bitapp_write_console_log_help(stdout: &mut io::Stdout) {
    let _ = stdout.write_all(
        b"commands: pgup, pgdn, top, bottom, refresh, help, exit\n\
          enter/pgdn: newer page; pgup: older page; exit: return to ram shell\n",
    );
    let _ = stdout.flush();
}

fn bitapp_write_console_log_bytes(stdout: &mut io::Stdout, bytes: &[u8]) {
    let mut output = [0u8; 1];
    for &byte in bytes {
        output[0] = match byte {
            b'\n' | b'\t' | 0x20..=0x7e => byte,
            b'\r' => b'\n',
            _ => b'?',
        };
        let _ = stdout.write_all(&output);
    }
}

fn bitapp_render_console_log_page(stdout: &mut io::Stdout, offset: usize) -> usize {
    let log_len = bitapp_console_log_len();
    let offset = bitapp_console_log_clamp_offset(offset, log_len);
    let mut page = [0u8; RAM_SHELL_LOG_PAGE_BYTES];
    let read_len = bitapp_read_console_log(offset, &mut page);
    let end = offset.saturating_add(read_len);

    let _ = stdout.write_all(b"\x0c");
    if log_len == 0 || read_len == 0 {
        let _ = writeln!(stdout, "console log is empty");
    } else {
        let _ = writeln!(
            stdout,
            "console log bytes {}..{} of {}",
            offset, end, log_len
        );
        bitapp_write_console_log_bytes(stdout, &page[..read_len]);
        if page[read_len - 1] != b'\n' {
            let _ = stdout.write_all(b"\n");
        }
        let _ = writeln!(
            stdout,
            "-- pgup older | pgdn newer | top | bottom | refresh | exit --"
        );
    }
    let _ = stdout.flush();

    offset
}

fn bitapp_run_console_log_command(
    stdout: &mut io::Stdout,
    command: &str,
    log_offset: &mut usize,
) -> bool {
    let command = command.trim();
    match command {
        "" | "pgdn" | "next" | "down" => {
            let log_len = bitapp_console_log_len();
            let max_offset = bitapp_console_log_max_offset(log_len);
            *log_offset = log_offset
                .saturating_add(RAM_SHELL_LOG_PAGE_BYTES)
                .min(max_offset);
            *log_offset = bitapp_render_console_log_page(stdout, *log_offset);
            true
        }
        "pgup" | "prev" | "up" => {
            *log_offset = log_offset.saturating_sub(RAM_SHELL_LOG_PAGE_BYTES);
            *log_offset = bitapp_render_console_log_page(stdout, *log_offset);
            true
        }
        "top" => {
            *log_offset = bitapp_render_console_log_page(stdout, 0);
            true
        }
        "bottom" | "end" => {
            *log_offset =
                bitapp_render_console_log_page(stdout, bitapp_console_log_bottom_offset());
            true
        }
        "refresh" | "r" => {
            *log_offset = bitapp_render_console_log_page(stdout, *log_offset);
            true
        }
        "help" | "?" => {
            bitapp_write_console_log_help(stdout);
            true
        }
        "exit" | "quit" | "q" => false,
        _ => {
            bitapp_write_console_log_help(stdout);
            true
        }
    }
}

fn parse_decimal(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as usize)?;
    }
    Some(value)
}

fn find_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().position(|&byte| byte == needle)
}

fn find_subslice(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > bytes.len() {
        return None;
    }
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_legacy_qa_records(mut bytes: &[u8], history: &mut Vec<QaRecord>) {
    loop {
        let Some(q_pos) = find_subslice(bytes, b"Q: ") else {
            break;
        };
        bytes = &bytes[q_pos + 3..];
        let Some(q_end) = find_byte(bytes, b'\n') else {
            break;
        };
        let question = trim_ascii(&bytes[..q_end]);
        bytes = &bytes[q_end + 1..];
        if !bytes.starts_with(b"A: ") {
            continue;
        }
        bytes = &bytes[3..];
        let a_end = find_byte(bytes, b'\n').unwrap_or(bytes.len());
        let answer = trim_ascii(&bytes[..a_end]);
        if !question.is_empty() && !answer.is_empty() {
            upsert_history_record(
                history,
                QaRecord {
                    question: question.to_vec(),
                    answer: answer.to_vec(),
                },
            );
        }
        if a_end >= bytes.len() {
            break;
        }
        bytes = &bytes[a_end + 1..];
    }
}

fn parse_structured_qa_records(bytes: &[u8], history: &mut Vec<QaRecord>) {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let rel_qa = find_subslice(&bytes[offset..], b"[RAMQA v1 q=");
        let rel_del = find_subslice(&bytes[offset..], b"[RAMDEL v1 k=");
        let mut next_record = None;
        for (rel, kind) in [(rel_qa, 0u8), (rel_del, 1u8)] {
            if let Some(candidate_start) = rel {
                if next_record
                    .map(|(current_start, _)| candidate_start < current_start)
                    .unwrap_or(true)
                {
                    next_record = Some((candidate_start, kind));
                }
            }
        }
        let Some((rel_start, record_kind)) = next_record else {
            parse_legacy_qa_records(&bytes[offset..], history);
            break;
        };

        let start = offset + rel_start;
        if start > offset {
            parse_legacy_qa_records(&bytes[offset..start], history);
        }

        if record_kind == 1 {
            let header_start = start + b"[RAMDEL v1 k=".len();
            let Some(header_end_rel) = find_byte(&bytes[header_start..], b'\n') else {
                break;
            };
            let header_end = header_start + header_end_rel;
            let header = &bytes[header_start..header_end];
            if header.last().copied() != Some(b']') {
                offset = start + 1;
                continue;
            }
            let Some(key_len) = parse_decimal(&header[..header.len() - 1]) else {
                offset = start + 1;
                continue;
            };
            let key_start = header_end + 1;
            let key_end = key_start.saturating_add(key_len);
            if key_end > bytes.len() {
                break;
            }
            let key = trim_ascii(&bytes[key_start..key_end]);
            if !key.is_empty() {
                let _ = forget_history_records(history, key);
            }
            offset = key_end.saturating_add(1).min(bytes.len());
            continue;
        }

        let header_start = start + b"[RAMQA v1 q=".len();
        let Some(header_end_rel) = find_byte(&bytes[header_start..], b'\n') else {
            break;
        };
        let header_end = header_start + header_end_rel;
        let header = &bytes[header_start..header_end];
        let Some(a_marker) = find_subslice(header, b" a=") else {
            offset = start + 1;
            continue;
        };
        if header.last().copied() != Some(b']') {
            offset = start + 1;
            continue;
        }
        let Some(q_len) = parse_decimal(&header[..a_marker]) else {
            offset = start + 1;
            continue;
        };
        let Some(a_len) = parse_decimal(&header[a_marker + 3..header.len() - 1]) else {
            offset = start + 1;
            continue;
        };

        let q_start = header_end + 1;
        let q_end = q_start.saturating_add(q_len);
        let a_start = q_end.saturating_add(1);
        let a_end = a_start.saturating_add(a_len);
        if a_end > bytes.len() {
            break;
        }
        upsert_history_record(
            history,
            QaRecord {
                question: bytes[q_start..q_end].to_vec(),
                answer: bytes[a_start..a_end].to_vec(),
            },
        );
        offset = a_end.saturating_add(1).min(bytes.len());
    }
}

fn bitapp_load_memory_db_history(history: &mut Vec<QaRecord>) {
    let mut scratch = vec![0u8; RAM_SHELL_HISTORY_CAP_BYTES];
    let ret = unsafe { sys_read_memory_db(scratch.as_mut_ptr().cast(), scratch.len()) };

    if ret < 0 {
        return;
    }

    let loaded = match usize::try_from(ret) {
        Ok(v) => v,
        Err(_) => return,
    };

    if loaded == 0 {
        return;
    }

    parse_structured_qa_records(&scratch[..loaded.min(scratch.len())], history);
    shrink_records(history, 0);
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if first == b' ' || first == b'\t' || first == b'\r' || first == b'\n' {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = bytes.split_last() {
        if last == b' ' || last == b'\t' || last == b'\r' || last == b'\n' {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

fn strip_prefix_once<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    bytes
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| trim_ascii(&bytes[prefix.len()..]))
}

fn trim_trailing_orphan_one(bytes: &[u8]) -> &[u8] {
    let answer = trim_ascii(bytes);
    if answer.len() < 4 {
        return answer;
    }

    let marker_start = if answer.last().copied() == Some(b'1') {
        answer.len() - 1
    } else if answer.len() >= 2
        && answer.last().copied() == Some(b'.')
        && answer.get(answer.len() - 2).copied() == Some(b'1')
    {
        answer.len() - 2
    } else {
        return answer;
    };

    if marker_start == 0 {
        return answer;
    }
    if !matches!(answer[marker_start - 1], b' ' | b'\t' | b'\r' | b'\n') {
        return answer;
    }

    let prefix = trim_ascii(&answer[..marker_start]);
    let prefix_nonspace = prefix
        .iter()
        .filter(|&&b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .count();
    if prefix_nonspace >= 8 {
        prefix
    } else {
        answer
    }
}

fn ram_shell_answer_bytes(generated: &[u8]) -> &[u8] {
    let mut answer = trim_ascii(generated);
    for prefix in [b"Answer:" as &[u8], b"Final answer:", b"A:"] {
        if let Some(stripped) = strip_prefix_once(answer, prefix) {
            answer = stripped;
            break;
        }
    }
    trim_trailing_orphan_one(answer)
}

fn bitapp_run_model_prompt(
    model_ptr: *const u8,
    model_len: usize,
    history: &mut Vec<QaRecord>,
    session_delta: &mut Vec<u8>,
    prompt: &str,
) -> Vec<u8> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        println!("bitapp: prompt is empty");
        return Vec::new();
    }

    let mut prompt_buf = [0u8; RAM_SHELL_LINE_LEN + 1];
    let prompt_len = build_raw_prompt(&mut prompt_buf, prompt);
    if prompt_len == 0 || prompt_len >= prompt_buf.len() {
        println!("bitapp: prompt buffer is full");
        return Vec::new();
    }

    BITAPP_GENERATION_ABORTED.store(false, Ordering::Release);
    let mut generated_output = [0u8; BITNET_OUTPUT_BUF_LEN];
    let ret = unsafe {
        hermit_bitnet_prompt_decode_from_buffer(
            model_ptr.cast::<core::ffi::c_void>(),
            model_len as u64,
            prompt_buf.as_ptr().cast(),
            BITNET_SHELL_N_PREDICT,
            bitapp_n_threads(),
            BITNET_SHELL_N_CTX,
            generated_output.as_mut_ptr().cast(),
            generated_output.len() as u64,
        )
    };
    if BITAPP_GENERATION_ABORTED.swap(false, Ordering::AcqRel) {
        println!("bitapp: RAM_SHELL prompt stopped by Ctrl-C");
    } else if ret == 0 {
        let generated_len = c_string_len(&generated_output);
        let answer = ram_shell_answer_bytes(&generated_output[..generated_len]);
        let answer_vec = answer.to_vec();
        let mut stdout = io::stdout();
        if !answer.is_empty() {
            let _ = stdout.write_all(answer);
        }
        if answer.last().copied() != Some(b'\n') {
            println!();
        }

        append_to_history(history, session_delta, prompt, answer);
        return answer_vec;
    }
    Vec::new()
}

fn bitapp_warm_bitnet_runtime(model_ptr: *const u8, model_len: usize) -> i32 {
    let mut warmup_output = [0u8; 1];
    unsafe {
        hermit_bitnet_prompt_decode_from_buffer(
            model_ptr.cast::<core::ffi::c_void>(),
            model_len as u64,
            BITNET_RUNTIME_WARMUP_PROMPT.as_ptr().cast(),
            -1,
            bitapp_n_threads(),
            BITNET_SHELL_N_CTX,
            warmup_output.as_mut_ptr().cast(),
            warmup_output.len() as u64,
        )
    }
}

fn bitapp_ram_shell(model_ptr: *const u8, model_len: usize, history: &mut Vec<QaRecord>) -> ! {
    let mut stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut line = [0u8; RAM_SHELL_LINE_LEN];
    let mut line_len = 0usize;
    let mut byte = [0u8; 1];
    let mut mode = RamShellMode::Command;
    let mut log_offset = 0usize;
    let mut session_delta = Vec::with_capacity(RAM_SHELL_HISTORY_CAP_BYTES);

    let _ = stdout.write_all(RAM_SHELL_COMMAND_PROMPT);
    let _ = stdout.flush();

    loop {
        match stdin.read(&mut byte) {
            Ok(0) => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(_) => {
                let ch = byte[0];
                match ch {
                    3 => {
                        let _ = stdout.write_all(b"^C\n");
                        line_len = 0;
                        if mode == RamShellMode::Log {
                            bitapp_console_log_set_capture(true);
                        }
                        mode = RamShellMode::Command;
                        let _ = stdout.write_all(RAM_SHELL_COMMAND_PROMPT);
                        let _ = stdout.flush();
                    }
                    b'\r' | b'\n' => {
                        let _ = stdout.write_all(b"\n");
                        let command = core::str::from_utf8(&line[..line_len]).unwrap_or("");
                        match mode {
                            RamShellMode::Command => {
                                let trimmed = command.trim();
                                if trimmed.is_empty() {
                                } else if trimmed == "help" {
                                    println!("commands: clear, reset, prompt, log, forget, erase, exit, reboot");
                                    println!(
                                        "prompt: enter prompt mode; prompt <text>: run immediately"
                                    );
                                    println!(
                                        "log: open captured console log; use pgup/pgdn/top/bottom/exit"
                                    );
                                } else if trimmed == "clear" {
                                    for _ in 0..40 {
                                        println!();
                                    }
                                } else if trimmed == "reset" {
                                    history.clear();
                                    session_delta.clear();
                                } else if trimmed == "erase memory" {
                                    bitapp_erase_memory_db(history, &mut session_delta);
                                } else if let Some(phrase) = trimmed
                                    .strip_prefix("forget ")
                                    .or_else(|| trimmed.strip_prefix("erase "))
                                {
                                    let _ = append_forget_to_history(
                                        history,
                                        &mut session_delta,
                                        phrase,
                                    );
                                } else if trimmed == "prompt" {
                                    mode = RamShellMode::Prompt;
                                } else if let Some(prompt) = trimmed.strip_prefix("prompt ") {
                                    bitapp_run_model_prompt(
                                        model_ptr,
                                        model_len,
                                        history,
                                        &mut session_delta,
                                        prompt,
                                    );
                                    mode = RamShellMode::Prompt;
                                } else if trimmed == "log" {
                                    bitapp_console_log_set_capture(false);
                                    log_offset = bitapp_console_log_bottom_offset();
                                    log_offset =
                                        bitapp_render_console_log_page(&mut stdout, log_offset);
                                    mode = RamShellMode::Log;
                                } else if trimmed == "exit" {
                                    bitapp_flush_memory_db(&mut session_delta);
                                    mode = RamShellMode::Command;
                                } else if trimmed == "reboot" {
                                    bitapp_reboot(&mut session_delta);
                                } else {
                                    println!("ram: {}", trimmed);
                                }
                            }
                            RamShellMode::Prompt => {
                                let prompt = command.trim();
                                if prompt == "cancel" {
                                    println!("bitapp: prompt cancelled");
                                } else if prompt == "exit" {
                                    bitapp_flush_memory_db(&mut session_delta);
                                    mode = RamShellMode::Command;
                                } else if prompt == "reboot" {
                                    bitapp_reboot(&mut session_delta);
                                } else if prompt == "erase memory" {
                                    bitapp_erase_memory_db(history, &mut session_delta);
                                } else if let Some(phrase) = prompt
                                    .strip_prefix("forget ")
                                    .or_else(|| prompt.strip_prefix("erase "))
                                {
                                    let _ = append_forget_to_history(
                                        history,
                                        &mut session_delta,
                                        phrase,
                                    );
                                } else if prompt == "clear" {
                                    for _ in 0..40 {
                                        println!();
                                    }
                                } else {
                                    bitapp_run_model_prompt(
                                        model_ptr,
                                        model_len,
                                        history,
                                        &mut session_delta,
                                        prompt,
                                    );
                                }
                            }
                            RamShellMode::Log => {
                                if !bitapp_run_console_log_command(
                                    &mut stdout,
                                    command,
                                    &mut log_offset,
                                ) {
                                    bitapp_console_log_set_capture(true);
                                    mode = RamShellMode::Command;
                                }
                            }
                        }
                        line_len = 0;
                        let prompt = match mode {
                            RamShellMode::Command => RAM_SHELL_COMMAND_PROMPT,
                            RamShellMode::Prompt => RAM_SHELL_MODEL_PROMPT,
                            RamShellMode::Log => RAM_SHELL_LOG_PROMPT,
                        };
                        let _ = stdout.write_all(prompt);
                        let _ = stdout.flush();
                    }
                    8 | 127 => {
                        if line_len > 0 {
                            line_len -= 1;
                            let _ = stdout.write_all(b"\x08 \x08");
                            let _ = stdout.flush();
                        }
                    }
                    0x20..=0x7e => {
                        if line_len + 1 < line.len() {
                            line[line_len] = ch;
                            line_len += 1;
                            let _ = stdout.write_all(&byte);
                            let _ = stdout.flush();
                        }
                    }
                    _ => {}
                }
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

pub(crate) fn run() {
    let app_start = bitapp_log_boot_event("bitapp_main_start");
    println!("bitapp: BitNet b1.58 Hermit linked app");

    let (model_ptr, model_len) = unsafe { (sys_get_model_ptr(), sys_get_model_len()) };
    let model_handoff = bitapp_log_boot_delta("model_handoff", app_start);
    println!(
        "bitapp: kernel model pointer={:?}, len={}",
        model_ptr, model_len
    );

    let mut history = Vec::new();
    bitapp_load_memory_db_history(&mut history);
    let warmup_start = bitapp_log_boot_delta("runtime_warmup_start", model_handoff);
    let warmup_ret = bitapp_warm_bitnet_runtime(model_ptr, model_len);
    let _warmup_done = bitapp_log_boot_delta("runtime_warmup_done", warmup_start);
    println!("bitapp: runtime warmup returned {}", warmup_ret);
    bitapp_ram_shell(model_ptr, model_len, &mut history);
}
