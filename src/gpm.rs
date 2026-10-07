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
use std::os::unix::net::UnixStream as StandardUnixStream;
use std::path::Path;

use crossterm::event::{
    Event as CrosstermEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use mio::net::UnixStream;
use mio::{Interest, Registry, Token};

const GPM_CONTROL_SOCKET: &str = "/dev/gpmctl";
const GPM_CONNECT_BYTES: usize = 16;
const GPM_EVENT_BYTES: usize = 28;
const GPM_MAGIC: u32 = 0x4770_6d4c;
const MAX_WHEEL_EVENTS_PER_PACKET: usize = 16;

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

/// Socket-only GPM mouse input registered with the terminal's shared reactor.
///
/// Construction is deliberately best-effort through [`Self::try_current`].
/// The caller owns readiness waiting and keyboard input. This adapter never
/// reads standard input or accesses Crossterm's synchronous event reader.
pub(crate) struct LinuxConsoleInput {
    client: GpmClient,
    pending_mouse: VecDeque<MouseEvent>,
}

impl LinuxConsoleInput {
    /// Connects only when the process is attached directly to `/dev/ttyN`.
    ///
    /// Missing sockets, inactive daemons, permissions, PTYs, and unsupported
    /// descriptor layouts all return `None`; callers retain their normal
    /// terminal-keyboard input path and may retry on an explicit F8 press.
    pub(crate) fn try_current(registry: &Registry, token: Token) -> Option<Self> {
        let virtual_console = current_virtual_console()?;
        Self::connect(
            Path::new(GPM_CONTROL_SOCKET),
            virtual_console,
            registry,
            token,
        )
        .ok()
    }

    /// Registers only the connected daemon socket; closing it releases registration.
    fn connect(
        socket: &Path,
        virtual_console: u32,
        registry: &Registry,
        token: Token,
    ) -> io::Result<Self> {
        let mut stream = StandardUnixStream::connect(socket)?;
        let pid = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        stream.write_all(&encode_connection(virtual_console, pid))?;
        stream.set_nonblocking(true)?;

        let mut client = GpmClient::new(UnixStream::from_std(stream));
        registry.register(client.stream_mut(), token, Interest::READABLE)?;

        Ok(Self {
            client,
            pending_mouse: VecDeque::new(),
        })
    }

    /// Drains mouse bytes after the caller observes this socket's readiness token.
    ///
    /// Reading stops at `WouldBlock`, retaining fragmented packets for later
    /// readiness. On EOF or another socket failure, complete events already
    /// decoded remain available through [`Self::read`] before the caller drops
    /// this adapter and falls back to keyboard-only input.
    pub(crate) fn drain_ready(&mut self) -> io::Result<()> {
        self.client.drain_ready(&mut self.pending_mouse)
    }

    /// Takes one buffered mouse event without waiting or reading any descriptor.
    pub(crate) fn read(&mut self) -> Option<CrosstermEvent> {
        self.pending_mouse.pop_front().map(CrosstermEvent::Mouse)
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
    use mio::{Events, Poll, Token};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// Builds a daemon substitute without touching stdin or the real GPM socket.
    fn socket_fixture(
        token: Token,
    ) -> (
        Poll,
        LinuxConsoleInput,
        StandardUnixStream,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().expect("temporary GPM socket directory");
        let path = directory.path().join("gpmctl");
        let listener = UnixListener::bind(&path).expect("fixture GPM listener");
        let poll = Poll::new().expect("shared input poll");
        let input = LinuxConsoleInput::connect(&path, 7, poll.registry(), token)
            .expect("connect registered GPM client");
        let (mut peer, _) = listener.accept().expect("fixture GPM connection");
        peer.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bounded handshake read");
        let mut request = [0; GPM_CONNECT_BYTES];
        peer.read_exact(&mut request)
            .expect("GPM connect handshake");
        assert_eq!(read_i32(&request, 12), 7);
        assert_eq!(
            read_i32(&request, 8),
            i32::try_from(std::process::id()).unwrap()
        );
        (poll, input, peer, directory)
    }

    /// Waits only for the fixture socket's readiness, with a bounded test deadline.
    fn await_socket(poll: &mut Poll, token: Token) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = Events::with_capacity(8);
        loop {
            let timeout = deadline.saturating_duration_since(Instant::now());
            assert!(!timeout.is_zero(), "fixture GPM socket never became ready");
            match poll.poll(&mut events, Some(timeout)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("fixture readiness failed: {error}"),
            }
            if events.iter().any(|event| event.token() == token) {
                return;
            }
        }
    }

    /// The parent's registry/token receive readiness; buffered mouse events need no new wait.
    #[test]
    fn gpm_socket_uses_parent_registry_and_buffers_coalesced_packets() {
        let token = Token(73);
        let (mut poll, mut input, mut peer, _directory) = socket_fixture(token);
        assert_eq!(input.read(), None);
        let packets = [
            encoded_event(fixture_event(GPM_DOWN, 4, 9)),
            encoded_event(fixture_event(GPM_UP, 4, 9)),
        ]
        .concat();
        peer.write_all(&packets).expect("coalesced GPM packets");
        await_socket(&mut poll, token);
        input.drain_ready().expect("drain registered socket");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert_eq!(
                input.read(),
                Some(CrosstermEvent::Mouse(mouse_event(
                    kind,
                    3,
                    8,
                    KeyModifiers::NONE
                )))
            );
        }
        assert_eq!(input.read(), None);
        input
            .drain_ready()
            .expect("an already drained socket is nonblocking");
    }

    /// Partial packet readiness does not become a synthetic mouse event or lose decoder state.
    #[test]
    fn gpm_socket_retains_fragmented_magic_prefixed_packet_until_complete() {
        let token = Token(91);
        let (mut poll, mut input, mut peer, _directory) = socket_fixture(token);
        let packet = [
            GPM_MAGIC.to_ne_bytes().as_slice(),
            encoded_event(fixture_event(GPM_MOVE, 8, 3)).as_slice(),
        ]
        .concat();
        for fragment in [&packet[..2], &packet[2..11]] {
            peer.write_all(fragment).expect("partial GPM packet");
            await_socket(&mut poll, token);
            input.drain_ready().expect("buffer partial packet");
            assert_eq!(input.read(), None);
        }
        peer.write_all(&packet[11..])
            .expect("final GPM packet fragment");
        await_socket(&mut poll, token);
        input.drain_ready().expect("decode complete packet");
        assert_eq!(
            input.read(),
            Some(CrosstermEvent::Mouse(mouse_event(
                MouseEventKind::Moved,
                7,
                2,
                KeyModifiers::NONE,
            )))
        );
        assert_eq!(input.read(), None);
    }

    /// A daemon disconnect is recoverable while complete packets already read remain available.
    #[test]
    fn gpm_socket_disconnect_retains_final_mouse_packet_and_allows_reconnect() {
        let token = Token(117);
        let (mut poll, mut input, mut peer, directory) = socket_fixture(token);
        peer.write_all(&encoded_event(fixture_event(GPM_DOWN, 2, 3)))
            .expect("final packet");
        drop(peer);
        await_socket(&mut poll, token);
        assert_eq!(
            input.drain_ready().unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(matches!(input.read(), Some(CrosstermEvent::Mouse(_))));
        assert_eq!(input.read(), None);
        drop(input);
        // A separate daemon socket models the caller's explicit F8 reconnect.
        let path = directory.path().join("restarted-gpmctl");
        let listener = UnixListener::bind(&path).expect("restarted daemon listener");
        let mut replacement = LinuxConsoleInput::connect(&path, 7, poll.registry(), token)
            .expect("reuse parent registry and token after disconnect");
        let (mut peer, _) = listener.accept().expect("replacement daemon client");
        peer.write_all(&encoded_event(fixture_event(GPM_MOVE, 5, 6)))
            .expect("replacement packet");
        await_socket(&mut poll, token);
        replacement
            .drain_ready()
            .expect("replacement socket remains usable");
        assert_eq!(
            replacement.read(),
            Some(CrosstermEvent::Mouse(mouse_event(
                MouseEventKind::Moved,
                4,
                5,
                KeyModifiers::NONE,
            )))
        );
    }

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
