//! Framing and metadata helpers for Kitty's OSC 5113 file-transfer protocol.
//!
//! The terminal and client each see a stream of bytes, so transfer commands can
//! be split at any byte boundary.  This module deliberately does not decode
//! file data or touch the filesystem.  The server only forwards complete,
//! bounded commands between a pane and the client-owned outer terminal.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const OSC_TRANSFER_CONTROL_KIND: &str = "terminal.transfer.v1";
pub const MAX_TRANSFER_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_TRANSFER_ID_BYTES: usize = 256;
pub const MAX_TRANSFER_SESSIONS: usize = 64;
pub const TRANSFER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
pub const TRANSFER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Identifies one live PTY even when a durable terminal id is reused on respawn.
/// A weak reference cannot keep the process alive and prevents allocator reuse
/// from making an old transfer look like the replacement runtime.
#[derive(Clone, Debug)]
pub(crate) struct TransferSource(std::sync::Weak<std::sync::atomic::AtomicU32>);

impl TransferSource {
    pub(crate) fn new(marker: &std::sync::Arc<std::sync::atomic::AtomicU32>) -> Self {
        Self(std::sync::Arc::downgrade(marker))
    }

    pub(crate) fn matches(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferPacket {
    pub raw: Vec<u8>,
    pub action: String,
    pub id: String,
}

/// A command emitted by the server for the outer terminal.  The bytes are
/// encoded because `EndpointControl.data` is a JSON string and transfer data
/// may contain arbitrary base64 text in future protocol revisions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferControl {
    pub boot_id: String,
    pub terminal_id: String,
    pub command: String,
    #[serde(default)]
    pub retire: bool,
}

impl TransferControl {
    pub fn new(boot_id: impl Into<String>, terminal_id: impl Into<String>, command: &[u8]) -> Self {
        Self {
            boot_id: boot_id.into(),
            terminal_id: terminal_id.into(),
            command: base64::engine::general_purpose::STANDARD.encode(command),
            retire: false,
        }
    }

    #[cfg(any(unix, test))]
    pub fn decode_command(&self) -> Option<Vec<u8>> {
        if self.command.len() > MAX_TRANSFER_COMMAND_BYTES.div_ceil(3) * 4 {
            return None;
        }
        base64::engine::general_purpose::STANDARD
            .decode(&self.command)
            .ok()
            .filter(|bytes| bytes.len() <= MAX_TRANSFER_COMMAND_BYTES)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum TransferOperation {
    Reply {
        terminal_id: String,
        session_id: String,
        data: String,
    },
    Cancel {
        terminal_id: String,
        session_id: String,
        message: String,
    },
}

/// A bounded scanner used for PTY output.  Non-transfer control strings are
/// consumed without retaining their payload; OSC 5113 packets are returned
/// only after BEL or ST.
#[derive(Debug, Default)]
pub struct TransferScanner {
    state: ScanState,
    utf8_remaining: u8,
}

#[derive(Debug, Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Osc(Vec<u8>),
    OscEscape(Vec<u8>),
    String,
    StringEscape,
    DiscardOsc(bool),
}

impl TransferScanner {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<TransferPacket> {
        let mut packets = Vec::new();
        for &byte in bytes {
            self.push_byte(byte, &mut packets);
        }
        packets
    }

    fn push_byte(&mut self, byte: u8, packets: &mut Vec<TransferPacket>) {
        let control = if utf8_continuation(&mut self.utf8_remaining, byte) {
            0
        } else {
            byte
        };
        if matches!(control, 0x18 | 0x1a) {
            self.state = ScanState::Ground;
            return;
        }
        let state = std::mem::replace(&mut self.state, ScanState::Ground);
        self.state = match state {
            ScanState::Ground => match control {
                0x1b => ScanState::Escape,
                0x9d => ScanState::Osc(vec![byte]),
                0x90 | 0x9e | 0x98 | 0x9f => ScanState::String,
                _ => ScanState::Ground,
            },
            ScanState::Escape => match byte {
                b']' => ScanState::Osc(vec![0x1b, byte]),
                b'P' | b'^' | b'_' | b'X' => ScanState::String,
                0x1b => ScanState::Escape,
                _ => ScanState::Ground,
            },
            ScanState::Osc(mut raw) => {
                if byte == 0x07 || control == 0x9c {
                    raw.push(byte);
                    emit_packet(raw, packets);
                    ScanState::Ground
                } else if byte == 0x1b {
                    raw.push(byte);
                    ScanState::OscEscape(raw)
                } else if raw.len() >= MAX_TRANSFER_COMMAND_BYTES {
                    ScanState::DiscardOsc(false)
                } else {
                    raw.push(byte);
                    if osc_prefix_is_invalid(&raw) {
                        ScanState::DiscardOsc(false)
                    } else {
                        ScanState::Osc(raw)
                    }
                }
            }
            ScanState::OscEscape(mut raw) => {
                if matches!(byte, b'\\' | 0x07) || control == 0x9c {
                    raw.push(byte);
                    emit_packet(raw, packets);
                    ScanState::Ground
                } else if raw.len() >= MAX_TRANSFER_COMMAND_BYTES.saturating_sub(1) {
                    ScanState::DiscardOsc(false)
                } else {
                    raw.push(byte);
                    if osc_prefix_is_invalid(&raw) {
                        ScanState::DiscardOsc(false)
                    } else {
                        ScanState::Osc(raw)
                    }
                }
            }
            ScanState::String => {
                if control == 0x9c {
                    ScanState::Ground
                } else if byte == 0x1b {
                    ScanState::StringEscape
                } else {
                    ScanState::String
                }
            }
            ScanState::StringEscape => {
                if byte == b'\\' || control == 0x9c {
                    ScanState::Ground
                } else if byte == 0x1b {
                    ScanState::StringEscape
                } else {
                    ScanState::String
                }
            }
            ScanState::DiscardOsc(escaped) => {
                if byte == 0x07 || control == 0x9c || (escaped && byte == b'\\') {
                    ScanState::Ground
                } else {
                    ScanState::DiscardOsc(byte == 0x1b)
                }
            }
        };
    }
}

fn utf8_continuation(remaining: &mut u8, byte: u8) -> bool {
    if *remaining > 0 && (0x80..=0xbf).contains(&byte) {
        *remaining -= 1;
        return true;
    }
    *remaining = match byte {
        0xc2..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf4 => 3,
        _ => 0,
    };
    false
}

fn emit_packet(raw: Vec<u8>, packets: &mut Vec<TransferPacket>) {
    if let Some((action, id)) = parse_action_and_id(&raw) {
        packets.push(TransferPacket { raw, action, id });
    }
}

/// Accept one complete protocol frame, never arbitrary terminal input. Values
/// are ASCII on the wire; binary data, paths and status text use base64.
pub fn parse_action_and_id(raw: &[u8]) -> Option<(String, String)> {
    let payload = transfer_payload(raw)?;
    let mut action = None;
    let mut id = None;
    for field in payload.split(|&byte| byte == b';') {
        let separator = field.iter().position(|&byte| byte == b'=')?;
        let (key, value) = field.split_at(separator);
        let value = &value[1..];
        if key.is_empty() || !key.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_') {
            return None;
        }
        // In particular reject embedded ESC/BEL/ST and concatenated frames.
        if !value.iter().all(|b| (0x20..=0x7e).contains(b)) {
            return None;
        }
        match key {
            b"ac" if action.is_none() => action = std::str::from_utf8(value).ok(),
            b"id" if id.is_none() => id = std::str::from_utf8(value).ok(),
            b"ac" | b"id" => return None,
            _ => {}
        }
    }
    let (action, id) = (action?, id?);
    if !matches!(
        action,
        "send" | "receive" | "file" | "data" | "end_data" | "status" | "finish" | "cancel"
    ) || id.is_empty()
        || id.len() > MAX_TRANSFER_ID_BYTES
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_:./@-".contains(&b))
    {
        return None;
    }
    Some((action.to_owned(), id.to_owned()))
}

fn transfer_payload(raw: &[u8]) -> Option<&[u8]> {
    if raw.len() > MAX_TRANSFER_COMMAND_BYTES {
        return None;
    }
    let payload = raw
        .strip_prefix(b"\x1b]5113;")
        .or_else(|| raw.strip_prefix(b"\x9d5113;"))?;
    payload
        .strip_suffix(b"\x07")
        .or_else(|| payload.strip_suffix(b"\x1b\\"))
        .or_else(|| payload.strip_suffix(b"\x9c"))
}

pub fn terminal_response(raw: &[u8], expected_id: &str) -> bool {
    parse_action_and_id(raw).is_some_and(|(action, id)| {
        id == expected_id && matches!(action.as_str(), "status" | "file" | "data" | "end_data")
    })
}

pub fn session_error(raw: &[u8]) -> bool {
    let Some(payload) = transfer_payload(raw) else {
        return false;
    };
    let mut status = None;
    for field in payload.split(|b| *b == b';') {
        if field.starts_with(b"fid=") {
            return false;
        }
        if let Some(value) = field.strip_prefix(b"st=") {
            status = decode_status(value);
        }
    }
    status.is_some_and(|status| !matches!(status.as_slice(), b"OK" | b"STARTED" | b"PROGRESS"))
}

fn decode_status(value: &[u8]) -> Option<Vec<u8>> {
    // Kitty's protocol encoder omits padding; both forms are valid on the wire.
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value))
        .ok()
}

#[cfg(any(unix, test))]
fn permission_denied(raw: &[u8]) -> bool {
    transfer_payload(raw)
        .into_iter()
        .flat_map(|payload| payload.split(|byte| *byte == b';'))
        .filter_map(|field| field.strip_prefix(b"st="))
        .filter_map(decode_status)
        .any(|value| value == b"EPERM" || value.starts_with(b"EPERM:"))
}

pub fn encode_failure(id: &str, message: &str) -> Vec<u8> {
    // Kitty's OSC 5113 status field uses unpadded Base64. Padding is accepted
    // when decoding terminal responses for compatibility, but must not be
    // emitted here because Kitty rejects a trailing `=` in `st`.
    let status = base64::engine::general_purpose::STANDARD_NO_PAD.encode(message);
    format!("\x1b]5113;ac=status;id={id};st={status}\x1b\\").into_bytes()
}

fn is_prefix(value: &[u8], expected: &[u8]) -> bool {
    value.len() <= expected.len() && expected.starts_with(value)
}

fn is_transfer_osc_prefix(value: &[u8]) -> bool {
    (value.len() >= b"\x1b]5113;".len() && value.starts_with(b"\x1b]5113;"))
        || (value.len() >= b"\x9d5113;".len() && value.starts_with(b"\x9d5113;"))
        || is_prefix(value, b"\x1b]5113;")
        || is_prefix(value, b"\x9d5113;")
}

fn osc_prefix_is_invalid(value: &[u8]) -> bool {
    !is_transfer_osc_prefix(value)
}

pub fn encode_cancel(id: &str) -> Vec<u8> {
    format!("\x1b]5113;ac=cancel;id={id}\x1b\\").into_bytes()
}

/// Client-side extractor output. Input chunks retain all non-transfer bytes,
/// including OSC sequences nested inside a bracketed paste or DCS/APC/PM/SOS.
#[cfg(any(unix, test))]
#[derive(Debug, PartialEq, Eq)]
pub enum ExtractedInput {
    Input(Vec<u8>),
    Transfer(TransferPacket),
}

#[cfg(any(unix, test))]
#[derive(Debug, Default)]
pub struct TransferInputExtractor {
    state: InputState,
    bracketed_paste: bool,
    utf8_remaining: u8,
    pending_failure: Option<TransferPacket>,
    outer_unfocused: bool,
    failure_waits_for_focus: bool,
}

#[cfg(any(unix, test))]
#[derive(Debug, Default)]
enum InputState {
    #[default]
    Ground,
    Escape(Vec<u8>),
    Csi(Vec<u8>),
    Osc(Vec<u8>),
    OscEscape(Vec<u8>),
    PassthroughOsc(bool),
    PassthroughString(bool),
    String(Vec<u8>),
    DiscardOsc(bool),
}

#[cfg(any(unix, test))]
impl TransferInputExtractor {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<ExtractedInput> {
        let mut output = Vec::new();
        let mut plain = Vec::new();
        for &byte in bytes {
            self.push_byte(byte, &mut plain, &mut output);
        }
        flush_plain(&mut plain, &mut output);
        self.order_permission_focus(output)
    }

    // Kitty restores focus asynchronously after reporting a denied permission
    // request. Keep the denial until its observed focus-loss/return cycle ends,
    // so kitten consumes the focus return before the denial makes it exit.
    // No focus event is suppressed, and a denial without focus loss uses only
    // the normal input idle boundary. Ordinary keys also release a held denial.
    fn order_permission_focus(&mut self, input: Vec<ExtractedInput>) -> Vec<ExtractedInput> {
        if self.pending_failure.is_none()
            && !input.iter().any(|item| match item {
                ExtractedInput::Transfer(packet) => {
                    packet.action == "status" && session_error(&packet.raw)
                }
                ExtractedInput::Input(bytes) => bytes == b"\x1b[I" || bytes == b"\x1b[O",
            })
        {
            return input;
        }
        let mut output = Vec::new();
        for item in input {
            match item {
                ExtractedInput::Transfer(packet)
                    if packet.action == "status" && session_error(&packet.raw) =>
                {
                    self.failure_waits_for_focus =
                        self.outer_unfocused && permission_denied(&packet.raw);
                    if let Some(previous) = self.pending_failure.replace(packet) {
                        output.push(ExtractedInput::Transfer(previous));
                    }
                }
                ExtractedInput::Input(bytes) if bytes == b"\x1b[I" => {
                    self.outer_unfocused = false;
                    self.failure_waits_for_focus = false;
                    output.push(ExtractedInput::Input(bytes));
                    if let Some(packet) = self.pending_failure.take() {
                        output.push(ExtractedInput::Transfer(packet));
                    }
                }
                ExtractedInput::Input(bytes) if bytes == b"\x1b[O" => {
                    self.outer_unfocused = true;
                    output.push(ExtractedInput::Input(bytes));
                }
                ExtractedInput::Input(bytes)
                    if self.failure_waits_for_focus
                        && bytes
                            .first()
                            .is_some_and(|byte| *byte == 0x1b || *byte >= 0x80) =>
                {
                    // Unrelated host control replies can arrive before focus returns.
                    output.push(ExtractedInput::Input(bytes));
                }
                item => {
                    self.failure_waits_for_focus = false;
                    if let Some(packet) = self.pending_failure.take() {
                        output.push(ExtractedInput::Transfer(packet));
                    }
                    output.push(item);
                }
            }
        }
        output
    }

    pub fn pending_mouse_escape(&self) -> bool {
        match &self.state {
            InputState::Escape(raw) => raw == b"\x1b",
            InputState::Csi(raw) => {
                raw.starts_with(b"\x1b[<")
                    && raw[3..]
                        .iter()
                        .all(|byte| byte.is_ascii_digit() || *byte == b';')
            }
            _ => false,
        }
    }

    /// Flush an ordinary incomplete host escape after the same idle interval
    /// used by the regular raw input framer. A partial OSC 5113 command is
    /// retained because it may continue in the next read.
    pub fn flush_timeout(&mut self) -> Vec<ExtractedInput> {
        let mut output = Vec::new();
        if !self.failure_waits_for_focus {
            if let Some(packet) = self.pending_failure.take() {
                output.push(ExtractedInput::Transfer(packet));
            }
        }
        if matches!(
            self.state,
            InputState::Escape(_) | InputState::Csi(_) | InputState::String(_)
        ) {
            let state = std::mem::replace(&mut self.state, InputState::Ground);
            match state {
                InputState::Escape(raw) | InputState::Csi(raw) => {
                    output.push(ExtractedInput::Input(raw))
                }
                InputState::String(raw) => {
                    self.state = InputState::PassthroughString(false);
                    output.push(ExtractedInput::Input(raw));
                }
                _ => {}
            }
        }
        output
    }

    fn push_byte(&mut self, byte: u8, plain: &mut Vec<u8>, output: &mut Vec<ExtractedInput>) {
        let control = if utf8_continuation(&mut self.utf8_remaining, byte) {
            0
        } else {
            byte
        };
        let state = std::mem::replace(&mut self.state, InputState::Ground);
        if matches!(control, 0x18 | 0x1a) && !self.bracketed_paste {
            match state {
                InputState::Escape(raw) | InputState::Csi(raw) | InputState::String(raw) => {
                    plain.extend_from_slice(&raw)
                }
                InputState::Osc(_) | InputState::OscEscape(_) | InputState::DiscardOsc(_) => return,
                _ => {}
            }
            plain.push(byte);
            return;
        }
        self.state = match state {
            InputState::Ground => {
                if byte == 0x1b {
                    InputState::Escape(vec![byte])
                } else if control == 0x9d && !self.bracketed_paste {
                    InputState::Osc(vec![byte])
                } else if matches!(control, 0x90 | 0x9e | 0x98 | 0x9f) && !self.bracketed_paste {
                    InputState::String(vec![byte])
                } else {
                    plain.push(byte);
                    InputState::Ground
                }
            }
            InputState::Escape(mut raw) => {
                if byte == 0x1b {
                    plain.extend_from_slice(&raw);
                    self.state = InputState::Escape(vec![byte]);
                    return;
                }
                raw.push(byte);
                if byte == b']' && !self.bracketed_paste {
                    InputState::Osc(raw)
                } else if byte == b'[' {
                    InputState::Csi(raw)
                } else if !self.bracketed_paste && matches!(byte, b'P' | b'^' | b'_' | b'X') {
                    InputState::String(raw)
                } else {
                    plain.extend_from_slice(&raw);
                    InputState::Ground
                }
            }
            InputState::Csi(mut raw) => {
                if byte == 0x1b {
                    plain.extend_from_slice(&raw);
                    self.state = InputState::Escape(vec![byte]);
                    return;
                }
                raw.push(byte);
                if (0x40..=0x7e).contains(&byte) {
                    if raw == b"\x1b[200~" {
                        self.bracketed_paste = true;
                    } else if raw == b"\x1b[201~" {
                        self.bracketed_paste = false;
                    }
                    if !self.bracketed_paste && matches!(raw.as_slice(), b"\x1b[I" | b"\x1b[O") {
                        flush_plain(plain, output);
                        output.push(ExtractedInput::Input(raw));
                    } else {
                        plain.extend_from_slice(&raw);
                    }
                    InputState::Ground
                } else if raw.len() >= 256 {
                    plain.extend_from_slice(&raw);
                    InputState::Ground
                } else {
                    InputState::Csi(raw)
                }
            }
            InputState::Osc(mut raw) => {
                if byte == 0x07 || control == 0x9c {
                    raw.push(byte);
                    finish_input_osc(raw, plain, output);
                    InputState::Ground
                } else if byte == 0x1b {
                    raw.push(byte);
                    InputState::OscEscape(raw)
                } else if raw.len() >= MAX_TRANSFER_COMMAND_BYTES {
                    InputState::DiscardOsc(false)
                } else {
                    raw.push(byte);
                    if osc_prefix_is_invalid(&raw) {
                        plain.extend_from_slice(&raw);
                        InputState::PassthroughOsc(false)
                    } else {
                        InputState::Osc(raw)
                    }
                }
            }
            InputState::OscEscape(mut raw) => {
                raw.push(byte);
                if matches!(byte, b'\\' | 0x07 | 0x9c) {
                    finish_input_osc(raw, plain, output);
                    InputState::Ground
                } else if osc_prefix_is_invalid(&raw) {
                    plain.extend_from_slice(&raw);
                    InputState::PassthroughOsc(false)
                } else if raw.len() >= MAX_TRANSFER_COMMAND_BYTES {
                    InputState::DiscardOsc(false)
                } else {
                    InputState::Osc(raw)
                }
            }
            InputState::PassthroughOsc(escaped) => {
                plain.push(byte);
                if byte == 0x07 || control == 0x9c || (escaped && byte == b'\\') {
                    InputState::Ground
                } else {
                    InputState::PassthroughOsc(byte == 0x1b)
                }
            }
            InputState::String(mut raw) => {
                raw.push(byte);
                plain.extend_from_slice(&raw);
                if control == 0x9c {
                    InputState::Ground
                } else if byte == 0x1b {
                    InputState::PassthroughString(true)
                } else {
                    InputState::PassthroughString(false)
                }
            }
            InputState::PassthroughString(escaped) => {
                plain.push(byte);
                if control == 0x9c || (escaped && byte == b'\\') {
                    InputState::Ground
                } else {
                    InputState::PassthroughString(byte == 0x1b)
                }
            }
            InputState::DiscardOsc(escaped) => {
                if byte == 0x07 || control == 0x9c || (escaped && byte == b'\\') {
                    InputState::Ground
                } else {
                    InputState::DiscardOsc(byte == 0x1b)
                }
            }
        };
    }
}

#[cfg(any(unix, test))]
fn finish_input_osc(raw: Vec<u8>, plain: &mut Vec<u8>, output: &mut Vec<ExtractedInput>) {
    if let Some((action, id)) = parse_action_and_id(&raw) {
        flush_plain(plain, output);
        output.push(ExtractedInput::Transfer(TransferPacket { raw, action, id }));
    } else if !raw.starts_with(b"\x1b]5113;") && !raw.starts_with(b"\x9d5113;") {
        plain.extend_from_slice(&raw);
    }
}

#[cfg(any(unix, test))]
fn flush_plain(plain: &mut Vec<u8>, output: &mut Vec<ExtractedInput>) {
    if !plain.is_empty() {
        output.push(ExtractedInput::Input(std::mem::take(plain)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(action: &str, id: &str) -> Vec<u8> {
        format!("\x1b]5113;ac={action};id={id}\x1b\\").into_bytes()
    }

    #[test]
    fn scanner_handles_fragmented_and_batched_commands() {
        let first = command("send", "one");
        let second = command("status", "one");
        let mut scanner = TransferScanner::default();
        assert!(scanner.push(&first[..7]).is_empty());
        let packets = scanner.push(&[&first[7..], &second[..]].concat());
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].action, "send");
        assert_eq!(packets[1].action, "status");
    }

    #[test]
    fn scanner_ignores_transfer_lookalikes_inside_dcs() {
        let embedded = b"\x1bPfoo\x1b]5113;ac=send;id=nope\x1b\\bar\x1b\\";
        assert!(TransferScanner::default().push(embedded).is_empty());
    }

    #[test]
    fn input_extractor_preserves_bracketed_paste_and_other_input() {
        let transfer = command("status", "one");
        let paste = [&b"\x1b[200~"[..], &transfer[..], b"\x1b[201~"].concat();
        let mut extractor = TransferInputExtractor::default();
        let output = extractor.push(&paste);
        assert_eq!(output, vec![ExtractedInput::Input(paste)]);
    }

    #[test]
    fn input_extractor_emits_transfer_and_retains_neighbors() {
        let transfer = command("status", "one");
        let bytes = [&b"before"[..], &transfer[..], b"after"].concat();
        let output = TransferInputExtractor::default().push(&bytes);
        assert!(matches!(&output[1], ExtractedInput::Transfer(packet) if packet.id == "one"));
        assert_eq!(output[0], ExtractedInput::Input(b"before".to_vec()));
        assert_eq!(output[2], ExtractedInput::Input(b"after".to_vec()));
    }

    #[test]
    fn oversized_commands_recover_at_terminator() {
        let mut scanner = TransferScanner::default();
        let mut bytes = b"\x1b]5113;ac=data;id=one;d=".to_vec();
        bytes.resize(MAX_TRANSFER_COMMAND_BYTES + 10, b'x');
        bytes.extend_from_slice(b"\x07");
        bytes.extend_from_slice(&command("status", "two"));
        let packets = scanner.push(&bytes);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].id, "two");
    }

    #[test]
    fn scanner_ignores_nontransfer_osc_without_retaining_payload() {
        let mut scanner = TransferScanner::default();
        let mut bytes = b"\x1b]0;title".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_TRANSFER_COMMAND_BYTES + 10));
        bytes.extend_from_slice(b"\x07");
        bytes.extend_from_slice(&command("status", "after"));
        let packets = scanner.push(&bytes);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].id, "after");
    }

    #[test]
    fn utf8_continuation_bytes_are_not_c1_controls() {
        let command = [b"\xc2".as_slice(), &[0x9d], b"text"].concat();
        assert!(TransferScanner::default().push(&command).is_empty());
        assert_eq!(
            TransferInputExtractor::default().push(&command),
            vec![ExtractedInput::Input(command)]
        );
    }

    #[test]
    fn input_extractor_flushes_lone_escape_but_keeps_transfer_prefix() {
        let mut extractor = TransferInputExtractor::default();
        assert!(extractor.push(b"\x1b").is_empty());
        assert_eq!(
            extractor.flush_timeout(),
            vec![ExtractedInput::Input(vec![0x1b])]
        );
        let mut extractor = TransferInputExtractor::default();
        assert!(extractor.push(b"\x1b]5113;ac=send;id=x").is_empty());
        assert!(extractor.flush_timeout().is_empty());
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[test]
    fn terminal_transfer_preserves_every_split_and_real_c1_frames() {
        for command in [
            b"\x1b]5113;ac=send;id=s;pw=sha256:abc\x1b\\".as_slice(),
            b"\x1b]5113;ac=status;id=s;st=T0s=\x07",
            b"\x9d5113;ac=receive;id=s\x9c",
        ] {
            for split in 0..=command.len() {
                let mut scanner = TransferScanner::default();
                let mut packets = scanner.push(&command[..split]);
                packets.extend(scanner.push(&command[split..]));
                assert_eq!(packets.len(), 1, "split {split}");
                assert_eq!(packets[0].raw, command);
                let mut input = TransferInputExtractor::default();
                let mut chunks = input.push(&command[..split]);
                chunks.extend(input.push(&command[split..]));
                assert_eq!(chunks, vec![ExtractedInput::Transfer(packets.remove(0))]);
            }
        }
    }

    #[test]
    fn terminal_transfer_rejects_injected_input_duplicate_ids_and_multiple_frames() {
        for bytes in [
            b"echo unsafe\r".as_slice(),
            b"\x1b]5113;ac=status;id=s;id=t\x07",
            b"\x1b]5113;ac=status;ac=send;id=s\x07",
            b"\x1b]5113;ac=unknown;id=s\x07",
            b"\x1b]5113;ac=status;id=s;d=x\x07echo unsafe\r\x07",
            b"\x1b]5113;ac=status;id=s\x07\x1b]5113;ac=status;id=s\x07",
        ] {
            assert!(parse_action_and_id(bytes).is_none(), "{bytes:?}");
        }
        assert!(!terminal_response(b"\x1b]5113;ac=status;id=s\x07", "other"));
        assert!(!terminal_response(b"\x1b]5113;ac=send;id=s\x07", "s"));
        assert!(terminal_response(
            b"\x1b]5113;ac=data;id=s;future=ok;d=YWJj\x07",
            "s"
        ));
    }

    #[test]
    fn terminal_transfer_discard_and_ignored_strings_survive_idle_and_utf8() {
        for prefix in [b"\x1bP".as_slice(), b"\x1b_", b"\x1b^", b"\x1bX"] {
            let lookalike = [prefix, b"\xc2\x9c\x1b]5113;ac=status;id=no\x07\x1b\x1b\\"].concat();
            let mut scanner = TransferScanner::default();
            assert!(scanner.push(&lookalike).is_empty());
            assert_eq!(scanner.push(b"\x1b]5113;ac=status;id=yes\x07").len(), 1);
            let mut input = TransferInputExtractor::default();
            let mut seen = input.push(prefix);
            seen.extend(input.flush_timeout());
            seen.extend(input.push(&lookalike[prefix.len()..]));
            let plain: Vec<_> = seen
                .into_iter()
                .flat_map(|item| match item {
                    ExtractedInput::Input(bytes) => bytes,
                    _ => panic!("nested transfer"),
                })
                .collect();
            assert_eq!(plain, lookalike);
        }
        let mut input = TransferInputExtractor::default();
        let mut oversized = b"\x1b]5113;ac=data;id=s;d=".to_vec();
        oversized.resize(MAX_TRANSFER_COMMAND_BYTES + 1, b'x');
        assert!(input.push(&oversized).is_empty());
        assert!(input.flush_timeout().is_empty());
        assert!(input.push(b"\x1b]5113;ac=status;id=no\x07").is_empty());
        assert!(matches!(
            &input.push(b"\x1b]5113;ac=status;id=yes\x07")[0],
            ExtractedInput::Transfer(_)
        ));
    }

    #[test]
    fn terminal_transfer_paste_with_unterminated_dcs_and_focus_reports_stays_input() {
        let bytes = b"\x1b[200~literal\x1bPtext\x1b[201~\x1b[I\x1b[O";
        let mut input = TransferInputExtractor::default();
        let mut reconstructed = Vec::new();
        for byte in bytes {
            for chunk in input.push(&[*byte]) {
                match chunk {
                    ExtractedInput::Input(bytes) => reconstructed.extend(bytes),
                    _ => panic!("paste extracted"),
                }
            }
        }
        assert_eq!(reconstructed, bytes);
        let mut input = TransferInputExtractor::default();
        let mut framer = crate::raw_input::RawInputByteFramer::for_host_input();
        assert!(input.push(b"\x1b").is_empty());
        for item in input.flush_timeout() {
            if let ExtractedInput::Input(bytes) = item {
                assert!(framer.push(&bytes).is_empty());
            }
        }
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn terminal_transfer_only_session_errors_end_the_route() {
        assert!(session_error(&encode_failure("s", "CANCELED")));
        assert!(session_error(&encode_failure("s", "EPERM:denied")));
        assert!(!session_error(&encode_failure("s", "OK")));
        assert!(!session_error(
            b"\x1b]5113;ac=status;id=s;fid=one;st=RVBFUk0=\x07"
        ));
    }

    #[test]
    fn terminal_transfer_failure_status_is_unpadded() {
        assert_eq!(
            encode_failure("s", "ENOTSUP:outer client does not support transfers"),
            b"\x1b]5113;ac=status;id=s;st=RU5PVFNVUDpvdXRlciBjbGllbnQgZG9lcyBub3Qgc3VwcG9ydCB0cmFuc2ZlcnM\x1b\\"
                .to_vec()
        );
    }

    #[test]
    fn terminal_transfer_recovers_after_cancelled_strings_and_adjacent_escape() {
        let command = b"\x1b]5113;ac=status;id=s;st=T0s=\x07";
        for prefix in [b"\x1b]5113;broken\x18".as_slice(), b"\x1bPignored\x1a"] {
            let bytes = [prefix, command].concat();
            assert_eq!(TransferScanner::default().push(&bytes).len(), 1);
            let items = TransferInputExtractor::default().push(&bytes);
            assert_eq!(
                items
                    .iter()
                    .filter(|item| matches!(item, ExtractedInput::Transfer(_)))
                    .count(),
                1
            );
        }
        let items = TransferInputExtractor::default().push(&[b"\x1b".as_slice(), command].concat());
        assert!(matches!(&items[0], ExtractedInput::Input(bytes) if bytes == b"\x1b"));
        assert!(matches!(&items[1], ExtractedInput::Transfer(_)));
    }

    #[test]
    fn terminal_transfer_permission_focus_precedes_exit_status_at_every_split() {
        let failure = encode_failure("s", "EPERM:denied");
        let bytes = [failure.as_slice(), b"\x1b[I"].concat();
        for split in 0..=bytes.len() {
            let mut extractor = TransferInputExtractor::default();
            let mut items = extractor.push(&bytes[..split]);
            items.extend(extractor.push(&bytes[split..]));
            assert_eq!(items.len(), 2, "split {split}");
            assert!(matches!(&items[0], ExtractedInput::Input(raw) if raw == b"\x1b[I"));
            assert!(matches!(&items[1], ExtractedInput::Transfer(packet) if packet.raw == failure));
            assert!(extractor.flush_timeout().is_empty());
            assert_eq!(
                extractor.push(b"\x1b[I"),
                vec![ExtractedInput::Input(b"\x1b[I".to_vec())],
                "later focus is never suppressed"
            );
        }
        let mut extractor = TransferInputExtractor::default();
        assert!(extractor.push(&failure).is_empty());
        assert!(
            matches!(&extractor.flush_timeout()[0], ExtractedInput::Transfer(packet) if packet.raw == failure)
        );
        let mut extractor = TransferInputExtractor::default();
        assert!(extractor.push(&failure).is_empty());
        let items = extractor.push(b"typed input");
        assert!(matches!(&items[0], ExtractedInput::Transfer(_)));
        assert!(matches!(&items[1], ExtractedInput::Input(raw) if raw == b"typed input"));
    }

    #[test]
    fn terminal_transfer_denial_waits_for_observed_focus_return_across_idle_gaps() {
        let mut extractor = TransferInputExtractor::default();
        assert_eq!(
            extractor.push(b"\x1b[O"),
            vec![ExtractedInput::Input(b"\x1b[O".to_vec())]
        );
        let denied = encode_failure("s", "EPERM:User refused the transfer");
        assert!(extractor.push(&denied).is_empty());
        for _ in 0..10 {
            assert!(extractor.flush_timeout().is_empty());
        }
        assert_eq!(
            extractor.push(b"\x1b[?997;1n"),
            vec![ExtractedInput::Input(b"\x1b[?997;1n".to_vec())]
        );
        let items = extractor.push(b"\x1b[I");
        assert!(matches!(&items[0], ExtractedInput::Input(bytes) if bytes == b"\x1b[I"));
        assert!(matches!(&items[1], ExtractedInput::Transfer(packet) if packet.raw == denied));
        assert!(extractor.flush_timeout().is_empty());
        extractor.push(b"\x1b[O");
        extractor.push(&encode_failure("s", "EIO:failed"));
        assert!(
            matches!(&extractor.flush_timeout()[0], ExtractedInput::Transfer(_)),
            "unrelated failure must not await focus"
        );
        extractor.push(&denied);
        assert!(
            matches!(&extractor.push(b"x")[0], ExtractedInput::Transfer(_)),
            "typing releases the denial before the key"
        );
    }

    #[test]
    fn terminal_transfer_preserves_mouse_escape_idle_policy() {
        let mut extractor = TransferInputExtractor::default();
        extractor.push(b"\x1b");
        assert!(extractor.pending_mouse_escape());
        extractor.push(b"[<35;12;");
        assert!(extractor.pending_mouse_escape());
        let mut framer = crate::raw_input::RawInputByteFramer::for_host_input();
        for item in extractor.flush_timeout() {
            if let ExtractedInput::Input(bytes) = item {
                assert!(framer.push(&bytes).is_empty());
            }
        }
        assert!(framer.flush_timeout().is_empty());
        let mut chunks = Vec::new();
        for item in extractor.push(b"7M") {
            if let ExtractedInput::Input(bytes) = item {
                chunks.extend(framer.push(&bytes));
            }
        }
        assert!(
            chunks.is_empty(),
            "the existing framer deliberately discards a timed-out mouse report"
        );
        assert_eq!(
            framer.push(b"x"),
            vec![b"x".to_vec()],
            "mouse tail must not leak into later typing"
        );
    }

    #[test]
    fn terminal_transfer_real_unpadded_statuses_drive_lifecycle_and_permission_focus() {
        let denied =
            b"\x1b]5113;ac=status;id=4c8fb1cd;st=RVBFUk06VXNlciByZWZ1c2VkIHRoZSB0cmFuc2Zlcg\x1b\\";
        assert!(session_error(denied));
        assert!(permission_denied(denied));
        assert!(session_error(
            b"\x1b]5113;ac=status;id=s;st=Q0FOQ0VMRUQ\x07"
        ));
        assert!(!session_error(b"\x1b]5113;ac=status;id=s;st=T0s\x07"));
        let mut extractor = TransferInputExtractor::default();
        extractor.push(b"\x1b[O");
        assert!(extractor.push(denied).is_empty());
        assert!(extractor.flush_timeout().is_empty());
        let items = extractor.push(b"\x1b[I");
        assert!(matches!(&items[0], ExtractedInput::Input(bytes) if bytes == b"\x1b[I"));
        assert!(matches!(&items[1], ExtractedInput::Transfer(packet) if packet.raw == denied));
    }
}
