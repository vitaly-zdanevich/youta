//! Safe GPM input for a real Linux virtual console.
//!
//! GPM exposes a cooked mouse-event stream through the `/dev/gpmctl` Unix
//! socket. The wire format is the native-endian `Gpm_Connect` and `Gpm_Event`
//! ABI documented by GPM. Youta encodes and decodes those fixed layouts
//! directly instead of linking `libgpm`, so the Cargo feature has no system
//! library dependency. Connecting is attempted only when one of the process
//! standard streams resolves to `/dev/ttyN`; terminal-emulator PTYs continue
//! to use Crossterm mouse reporting exclusively.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream as StandardUnixStream;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{
    self, Event as CrosstermEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use mio::net::UnixStream;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};

const GPM_CONTROL_SOCKET: &str = "/dev/gpmctl";
const GPM_CONNECT_BYTES: usize = 16;
const GPM_EVENT_BYTES: usize = 28;
const GPM_MAGIC: u32 = 0x4770_6d4c;
const MAX_WHEEL_EVENTS_PER_PACKET: usize = 16;
const STDIN_FD: RawFd = 0;
const STDIN_TOKEN: Token = Token(0);
const GPM_TOKEN: Token = Token(1);

const GPM_MOVE: u32 = 1;
const GPM_DRAG: u32 = 2;
const GPM_DOWN: u32 = 4;
const GPM_UP: u32 = 8;

const GPM_BUTTON_RIGHT: u8 = 1;
const GPM_BUTTON_MIDDLE: u8 = 2;
const GPM_BUTTON_LEFT: u8 = 4;
const GPM_BUTTON_WHEEL_UP: u8 = 16;
const GPM_BUTTON_WHEEL_DOWN: u8 = 32;

const GPM_MOD_SHIFT: u8 = 1 << 0;
const GPM_MOD_ALT_GR: u8 = 1 << 1;
const GPM_MOD_CONTROL: u8 = 1 << 2;
const GPM_MOD_ALT: u8 = 1 << 3;
const GPM_MOD_SHIFT_LEFT: u8 = 1 << 4;
const GPM_MOD_SHIFT_RIGHT: u8 = 1 << 5;
const GPM_MOD_CONTROL_LEFT: u8 = 1 << 6;
const GPM_MOD_CONTROL_RIGHT: u8 = 1 << 7;

/// Readiness multiplexer for Crossterm input and one GPM control socket.
///
/// Construction is deliberately best-effort through [`Self::try_current`].
/// Once connected, `mio` waits on both standard input and `/dev/gpmctl`
/// without a timer-driven busy loop.
pub(crate) struct LinuxConsoleInput {
    poll: Poll,
    readiness: Events,
    client: GpmClient,
    pending_mouse: VecDeque<MouseEvent>,
    terminal_ready: bool,
}

impl LinuxConsoleInput {
    /// Connects only when the process is attached directly to `/dev/ttyN`.
    ///
    /// Missing sockets, inactive daemons, permissions, PTYs, and unsupported
    /// descriptor layouts all return `None`; callers should silently retain
    /// their normal terminal-keyboard input path.
    pub(crate) fn try_current() -> Option<Self> {
        let virtual_console = current_virtual_console()?;
        Self::connect(Path::new(GPM_CONTROL_SOCKET), virtual_console).ok()
    }

    fn connect(socket: &Path, virtual_console: u32) -> io::Result<Self> {
        let mut stream = StandardUnixStream::connect(socket)?;
        let pid = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        stream.write_all(&encode_connection(virtual_console, pid))?;
        stream.set_nonblocking(true)?;

        let mut client = GpmClient::new(UnixStream::from_std(stream));
        let poll = Poll::new()?;
        let mut stdin = SourceFd(&STDIN_FD);
        poll.registry()
            .register(&mut stdin, STDIN_TOKEN, Interest::READABLE)?;
        poll.registry()
            .register(client.stream_mut(), GPM_TOKEN, Interest::READABLE)?;

        Ok(Self {
            poll,
            readiness: Events::with_capacity(8),
            client,
            pending_mouse: VecDeque::new(),
            terminal_ready: false,
        })
    }

    /// Waits until either Crossterm or GPM has an input event.
    pub(crate) fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        if self.terminal_ready || !self.pending_mouse.is_empty() {
            return Ok(true);
        }

        // Crossterm may already hold a decoded key or a SIGWINCH-generated
        // resize that is not represented by standard-input readiness.
        if event::poll(Duration::ZERO)? {
            self.terminal_ready = true;
            return Ok(true);
        }

        self.readiness.clear();
        self.poll.poll(&mut self.readiness, Some(timeout))?;
        let mut stdin_ready = false;
        let mut gpm_ready = false;
        for ready in &self.readiness {
            match ready.token() {
                STDIN_TOKEN => stdin_ready = true,
                GPM_TOKEN => gpm_ready = true,
                _ => {}
            }
        }

        if gpm_ready {
            self.client.drain_ready(&mut self.pending_mouse)?;
        }
        if stdin_ready {
            self.terminal_ready = true;
        } else if event::poll(Duration::ZERO)? {
            // Catch a resize that arrived while `mio` was blocked.
            self.terminal_ready = true;
        }

        Ok(self.terminal_ready || !self.pending_mouse.is_empty())
    }

    /// Reads the input whose readiness was reported by [`Self::poll`].
    pub(crate) fn read(&mut self) -> io::Result<CrosstermEvent> {
        if self.terminal_ready {
            self.terminal_ready = false;
            return event::read();
        }
        self.pending_mouse
            .pop_front()
            .map(CrosstermEvent::Mouse)
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))
    }
}

struct GpmClient {
    stream: UnixStream,
    decoder: GpmEventDecoder,
    mapper: GpmMouseMapper,
}

impl GpmClient {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            decoder: GpmEventDecoder::default(),
            mapper: GpmMouseMapper::default(),
        }
    }

    fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    fn drain_ready(&mut self, pending: &mut VecDeque<MouseEvent>) -> io::Result<()> {
        let mut bytes = [0_u8; 512];
        loop {
            match self.stream.read(&mut bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "GPM closed /dev/gpmctl",
                    ));
                }
                Ok(count) => {
                    for event in self.decoder.push(&bytes[..count]) {
                        self.mapper.map_into(event, pending);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// One decoded native GPM event packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GpmEvent {
    buttons: u8,
    modifiers: u8,
    virtual_console: u16,
    delta_x: i16,
    delta_y: i16,
    x: i16,
    y: i16,
    event_type: u32,
    clicks: i32,
    margin: i32,
    wheel_x: i16,
    wheel_y: i16,
}

/// Incremental decoder for fragmented or coalesced GPM event packets.
///
/// Distribution builds normally use the 28-byte packet. The decoder also
/// accepts the optional four-byte `GPM_MAGIC` prefix used by some GPM builds.
#[derive(Debug, Default)]
struct GpmEventDecoder {
    buffered: Vec<u8>,
}

impl GpmEventDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<GpmEvent> {
        self.buffered.extend_from_slice(bytes);
        let mut decoded = Vec::new();
        loop {
            let has_magic = self
                .buffered
                .get(..GPM_MAGIC.to_ne_bytes().len())
                .is_some_and(|prefix| prefix == GPM_MAGIC.to_ne_bytes());
            let packet_bytes = GPM_EVENT_BYTES
                + if has_magic {
                    GPM_MAGIC.to_ne_bytes().len()
                } else {
                    0
                };
            if self.buffered.len() < packet_bytes {
                break;
            }
            let event_start = if has_magic {
                GPM_MAGIC.to_ne_bytes().len()
            } else {
                0
            };
            decoded.push(decode_event(
                &self.buffered[event_start..event_start + GPM_EVENT_BYTES],
            ));
            self.buffered.drain(..packet_bytes);
        }
        decoded
    }
}

#[derive(Debug, Default)]
struct GpmMouseMapper {
    pressed_button: Option<MouseButton>,
}

impl GpmMouseMapper {
    fn map_into(&mut self, event: GpmEvent, pending: &mut VecDeque<MouseEvent>) {
        let column = zero_based_coordinate(event.x);
        let row = zero_based_coordinate(event.y);
        let modifiers = map_modifiers(event.modifiers);

        if event.wheel_y != 0 {
            let kind = if event.wheel_y > 0 {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            };
            push_wheel_events(event.wheel_y, kind, column, row, modifiers, pending);
            return;
        }
        if event.wheel_x != 0 {
            let kind = if event.wheel_x > 0 {
                MouseEventKind::ScrollRight
            } else {
                MouseEventKind::ScrollLeft
            };
            push_wheel_events(event.wheel_x, kind, column, row, modifiers, pending);
            return;
        }
        if event.buttons & GPM_BUTTON_WHEEL_UP != 0 {
            pending.push_back(mouse_event(
                MouseEventKind::ScrollUp,
                column,
                row,
                modifiers,
            ));
            return;
        }
        if event.buttons & GPM_BUTTON_WHEEL_DOWN != 0 {
            pending.push_back(mouse_event(
                MouseEventKind::ScrollDown,
                column,
                row,
                modifiers,
            ));
            return;
        }

        let packet_button = map_button(event.buttons);
        let kind = if event.event_type & GPM_DOWN != 0 {
            let button = packet_button.unwrap_or(MouseButton::Left);
            self.pressed_button = Some(button);
            MouseEventKind::Down(button)
        } else if event.event_type & GPM_UP != 0 {
            let button = packet_button
                .or(self.pressed_button.take())
                .unwrap_or(MouseButton::Left);
            self.pressed_button = None;
            MouseEventKind::Up(button)
        } else if event.event_type & GPM_DRAG != 0 {
            let button = packet_button
                .or(self.pressed_button)
                .unwrap_or(MouseButton::Left);
            self.pressed_button = Some(button);
            MouseEventKind::Drag(button)
        } else if event.event_type & GPM_MOVE != 0 {
            MouseEventKind::Moved
        } else {
            return;
        };
        pending.push_back(mouse_event(kind, column, row, modifiers));
    }
}

fn push_wheel_events(
    distance: i16,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
    pending: &mut VecDeque<MouseEvent>,
) {
    for _ in 0..usize::from(distance.unsigned_abs()).min(MAX_WHEEL_EVENTS_PER_PACKET) {
        pending.push_back(mouse_event(kind, column, row, modifiers));
    }
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }
}

fn map_button(buttons: u8) -> Option<MouseButton> {
    if buttons & GPM_BUTTON_LEFT != 0 {
        Some(MouseButton::Left)
    } else if buttons & GPM_BUTTON_MIDDLE != 0 {
        Some(MouseButton::Middle)
    } else if buttons & GPM_BUTTON_RIGHT != 0 {
        Some(MouseButton::Right)
    } else {
        None
    }
}

fn map_modifiers(modifiers: u8) -> KeyModifiers {
    let mut mapped = KeyModifiers::empty();
    if modifiers & (GPM_MOD_SHIFT | GPM_MOD_SHIFT_LEFT | GPM_MOD_SHIFT_RIGHT) != 0 {
        mapped.insert(KeyModifiers::SHIFT);
    }
    if modifiers & (GPM_MOD_CONTROL | GPM_MOD_CONTROL_LEFT | GPM_MOD_CONTROL_RIGHT) != 0 {
        mapped.insert(KeyModifiers::CONTROL);
    }
    if modifiers & (GPM_MOD_ALT | GPM_MOD_ALT_GR) != 0 {
        mapped.insert(KeyModifiers::ALT);
    }
    mapped
}

fn zero_based_coordinate(coordinate: i16) -> u16 {
    u16::try_from(coordinate.saturating_sub(1)).unwrap_or_default()
}

/// Encodes GPM's native-endian 16-byte `Gpm_Connect` request.
fn encode_connection(virtual_console: u32, pid: i32) -> [u8; GPM_CONNECT_BYTES] {
    let mut encoded = [0_u8; GPM_CONNECT_BYTES];
    encoded[0..2].copy_from_slice(&u16::MAX.to_ne_bytes());
    encoded[2..4].copy_from_slice(&u16::MAX.to_ne_bytes());
    encoded[4..6].copy_from_slice(&0_u16.to_ne_bytes());
    encoded[6..8].copy_from_slice(&u16::MAX.to_ne_bytes());
    encoded[8..12].copy_from_slice(&pid.to_ne_bytes());
    encoded[12..16].copy_from_slice(
        &i32::try_from(virtual_console)
            .unwrap_or(i32::MAX)
            .to_ne_bytes(),
    );
    encoded
}

fn decode_event(packet: &[u8]) -> GpmEvent {
    GpmEvent {
        buttons: packet[0],
        modifiers: packet[1],
        virtual_console: read_u16(packet, 2),
        delta_x: read_i16(packet, 4),
        delta_y: read_i16(packet, 6),
        x: read_i16(packet, 8),
        y: read_i16(packet, 10),
        event_type: read_u32(packet, 12),
        clicks: read_i32(packet, 16),
        margin: read_i32(packet, 20),
        wheel_x: read_i16(packet, 24),
        wheel_y: read_i16(packet, 26),
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_ne_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_i16(bytes: &[u8], offset: usize) -> i16 {
    i16::from_ne_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_ne_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn current_virtual_console() -> Option<u32> {
    [0_u8, 1, 2].into_iter().find_map(|descriptor| {
        let target = fs::read_link(format!("/proc/self/fd/{descriptor}")).ok()?;
        virtual_console_from_path(&target)
    })
}

fn virtual_console_from_path(path: &Path) -> Option<u32> {
    if path.parent() != Some(Path::new("/dev")) {
        return None;
    }
    let suffix = path.file_name()?.to_str()?.strip_prefix("tty")?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let virtual_console = suffix.parse().ok()?;
    (virtual_console > 0).then_some(virtual_console)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn encoded_event(event: GpmEvent) -> [u8; GPM_EVENT_BYTES] {
        let mut encoded = [0_u8; GPM_EVENT_BYTES];
        encoded[0] = event.buttons;
        encoded[1] = event.modifiers;
        encoded[2..4].copy_from_slice(&event.virtual_console.to_ne_bytes());
        encoded[4..6].copy_from_slice(&event.delta_x.to_ne_bytes());
        encoded[6..8].copy_from_slice(&event.delta_y.to_ne_bytes());
        encoded[8..10].copy_from_slice(&event.x.to_ne_bytes());
        encoded[10..12].copy_from_slice(&event.y.to_ne_bytes());
        encoded[12..16].copy_from_slice(&event.event_type.to_ne_bytes());
        encoded[16..20].copy_from_slice(&event.clicks.to_ne_bytes());
        encoded[20..24].copy_from_slice(&event.margin.to_ne_bytes());
        encoded[24..26].copy_from_slice(&event.wheel_x.to_ne_bytes());
        encoded[26..28].copy_from_slice(&event.wheel_y.to_ne_bytes());
        encoded
    }

    fn fixture_event(event_type: u32, x: i16, y: i16) -> GpmEvent {
        GpmEvent {
            buttons: GPM_BUTTON_LEFT,
            modifiers: 0,
            virtual_console: 7,
            delta_x: 0,
            delta_y: 0,
            x,
            y,
            event_type,
            clicks: 0,
            margin: 0,
            wheel_x: 0,
            wheel_y: 0,
        }
    }

    #[test]
    fn connection_request_matches_native_gpm_layout() {
        let encoded = encode_connection(7, 12_345);

        assert_eq!(read_u16(&encoded, 0), u16::MAX);
        assert_eq!(read_u16(&encoded, 2), u16::MAX);
        assert_eq!(read_u16(&encoded, 4), 0);
        assert_eq!(read_u16(&encoded, 6), u16::MAX);
        assert_eq!(read_i32(&encoded, 8), 12_345);
        assert_eq!(read_i32(&encoded, 12), 7);
    }

    #[test]
    fn decoder_accepts_fragmented_coalesced_and_magic_prefixed_packets() {
        let first = fixture_event(GPM_MOVE, 11, 5);
        let second = fixture_event(GPM_DOWN, 20, 9);
        let first_bytes = encoded_event(first);
        let second_bytes = encoded_event(second);
        let mut decoder = GpmEventDecoder::default();

        assert!(decoder.push(&first_bytes[..9]).is_empty());
        let mut remainder = Vec::new();
        remainder.extend_from_slice(&first_bytes[9..]);
        remainder.extend_from_slice(&GPM_MAGIC.to_ne_bytes());
        remainder.extend_from_slice(&second_bytes);
        assert_eq!(decoder.push(&remainder), vec![first, second]);
    }

    #[test]
    fn mapper_preserves_coordinates_buttons_modifiers_and_wheel_distance() {
        let mut mapper = GpmMouseMapper::default();
        let mut pending = VecDeque::new();
        let mut down = fixture_event(GPM_DOWN, 1, 2);
        down.buttons = GPM_BUTTON_RIGHT;
        down.modifiers = GPM_MOD_SHIFT_LEFT | GPM_MOD_CONTROL | GPM_MOD_ALT;
        mapper.map_into(down, &mut pending);

        let mut up = fixture_event(GPM_UP, 1, 2);
        up.buttons = 0;
        mapper.map_into(up, &mut pending);

        let mut wheel = fixture_event(GPM_MOVE, 5, 4);
        wheel.buttons = 0;
        wheel.wheel_y = -2;
        mapper.map_into(wheel, &mut pending);

        assert_eq!(
            pending.pop_front(),
            Some(mouse_event(
                MouseEventKind::Down(MouseButton::Right),
                0,
                1,
                KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT,
            ))
        );
        assert_eq!(
            pending.pop_front().map(|event| event.kind),
            Some(MouseEventKind::Up(MouseButton::Right))
        );
        assert_eq!(
            pending
                .into_iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![MouseEventKind::ScrollDown, MouseEventKind::ScrollDown]
        );
    }

    #[test]
    fn mapper_converts_motion_and_button_drag_to_crossterm_semantics() {
        let mut mapper = GpmMouseMapper::default();
        let mut pending = VecDeque::new();
        let mut moved = fixture_event(GPM_MOVE, 8, 3);
        moved.buttons = 0;
        mapper.map_into(moved, &mut pending);
        mapper.map_into(fixture_event(GPM_DOWN, 8, 3), &mut pending);

        let mut dragged = fixture_event(GPM_DRAG, 9, 4);
        dragged.buttons = 0;
        mapper.map_into(dragged, &mut pending);

        assert_eq!(
            pending.into_iter().collect::<Vec<_>>(),
            vec![
                mouse_event(MouseEventKind::Moved, 7, 2, KeyModifiers::NONE),
                mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    7,
                    2,
                    KeyModifiers::NONE,
                ),
                mouse_event(
                    MouseEventKind::Drag(MouseButton::Left),
                    8,
                    3,
                    KeyModifiers::NONE,
                ),
            ]
        );
    }

    #[test]
    fn virtual_console_detection_rejects_ptys_aliases_and_tty_zero() {
        assert_eq!(
            virtual_console_from_path(&PathBuf::from("/dev/tty12")),
            Some(12)
        );
        assert_eq!(
            virtual_console_from_path(&PathBuf::from("/dev/pts/12")),
            None
        );
        assert_eq!(virtual_console_from_path(&PathBuf::from("/dev/tty")), None);
        assert_eq!(virtual_console_from_path(&PathBuf::from("/dev/tty0")), None);
        assert_eq!(
            virtual_console_from_path(&PathBuf::from("/tmp/dev/tty12")),
            None
        );
    }
}
