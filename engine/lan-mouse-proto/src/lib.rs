use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// edge fraction is not finite or outside `0.0..=1.0`
    #[error("invalid edge fraction: `{0}`")]
    InvalidFraction(f64),
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    /// the optional fraction is the normalized position (`0.0..=1.0`) along the
    /// entered edge, used to place the pointer at the crossing point
    Enter(Position, Option<f64>),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    /// the optional fraction is the normalized position along the edge the
    /// cursor returns at, used to place the pointer on capture release
    Leave(u32, Option<f64>),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s, frac) => write!(f, "Enter({s}, {frac:?})"),
            ProtoEvent::Leave(s, frac) => write!(f, "Leave({s}, {frac:?})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(..) => EventType::Enter,
            ProtoEvent::Leave(..) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(
                decode_u8(&mut buf)?.try_into()?,
                decode_fraction(&mut buf)?,
            )),
            EventType::Leave => Ok(Self::Leave(
                decode_u32(&mut buf)?,
                decode_fraction(&mut buf)?,
            )),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos, fraction) => {
                    encode_u8(buf, len, pos as u8);
                    encode_fraction(buf, len, fraction);
                }
                ProtoEvent::Leave(serial, fraction) => {
                    encode_u32(buf, len, serial);
                    encode_fraction(buf, len, fraction);
                }
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

/// encode an optional edge fraction as a presence flag followed by an f64
/// payload (0.0 when absent) so the wire size stays constant
fn encode_fraction(buf: &mut &mut [u8], amt: &mut usize, fraction: Option<f64>) {
    encode_u8(buf, amt, fraction.is_some() as u8);
    encode_f64(buf, amt, fraction.unwrap_or(0.0));
}

/// decode an optional edge fraction; a present fraction must be finite and
/// within `0.0..=1.0`, otherwise the datagram is rejected
fn decode_fraction(data: &mut &[u8]) -> Result<Option<f64>, ProtocolError> {
    let present = decode_u8(data)? != 0;
    let fraction = decode_f64(data)?;
    if !present {
        return Ok(None);
    }
    if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
        return Err(ProtocolError::InvalidFraction(fraction));
    }
    Ok(Some(fraction))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(event: ProtoEvent) -> ProtoEvent {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        assert!(len <= MAX_EVENT_SIZE);
        ProtoEvent::try_from(buf).expect("roundtrip")
    }

    fn enter_buf(pos: Position, flag: u8, fraction: f64) -> [u8; MAX_EVENT_SIZE] {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        buf[0] = EventType::Enter as u8;
        buf[1] = pos as u8;
        buf[2] = flag;
        buf[3..11].copy_from_slice(&fraction.to_be_bytes());
        buf
    }

    fn leave_buf(serial: u32, flag: u8, fraction: f64) -> [u8; MAX_EVENT_SIZE] {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        buf[0] = EventType::Leave as u8;
        buf[1..5].copy_from_slice(&serial.to_be_bytes());
        buf[5] = flag;
        buf[6..14].copy_from_slice(&fraction.to_be_bytes());
        buf
    }

    fn expect_fraction(event: ProtoEvent) -> Option<f64> {
        match event {
            ProtoEvent::Enter(_, f) => f,
            ProtoEvent::Leave(_, f) => f,
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn enter_roundtrips_with_and_without_edge_fraction() {
        for pos in [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            match roundtrip(ProtoEvent::Enter(pos, Some(0.42))) {
                ProtoEvent::Enter(p, f) => {
                    assert!(matches!(p, _p if p as u8 == pos as u8));
                    assert_eq!(f, Some(0.42));
                }
                other => panic!("unexpected event: {other:?}"),
            }
            match roundtrip(ProtoEvent::Enter(pos, None)) {
                ProtoEvent::Enter(_, f) => assert_eq!(f, None),
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[test]
    fn enter_fraction_boundaries_roundtrip_exactly() {
        assert_eq!(
            expect_fraction(roundtrip(ProtoEvent::Enter(Position::Left, Some(0.0)))),
            Some(0.0)
        );
        assert_eq!(
            expect_fraction(roundtrip(ProtoEvent::Enter(Position::Left, Some(1.0)))),
            Some(1.0)
        );
    }

    #[test]
    fn leave_roundtrips_serial_and_edge_fraction() {
        match roundtrip(ProtoEvent::Leave(7, Some(0.5))) {
            ProtoEvent::Leave(serial, f) => {
                assert_eq!(serial, 7);
                assert_eq!(f, Some(0.5));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        match roundtrip(ProtoEvent::Leave(u32::MAX, None)) {
            ProtoEvent::Leave(serial, f) => {
                assert_eq!(serial, u32::MAX);
                assert_eq!(f, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn ack_roundtrip_unchanged() {
        match roundtrip(ProtoEvent::Ack(123)) {
            ProtoEvent::Ack(serial) => assert_eq!(serial, 123),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn nonfinite_and_out_of_range_enter_fractions_are_rejected() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.001, 1.001] {
            assert!(
                matches!(
                    ProtoEvent::try_from(enter_buf(Position::Left, 1, bad)),
                    Err(ProtocolError::InvalidFraction(_))
                ),
                "fraction {bad} must be rejected"
            );
        }
    }

    #[test]
    fn nonfinite_and_out_of_range_leave_fractions_are_rejected() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5, 2.0] {
            assert!(
                matches!(
                    ProtoEvent::try_from(leave_buf(0, 1, bad)),
                    Err(ProtocolError::InvalidFraction(_))
                ),
                "fraction {bad} must be rejected"
            );
        }
    }

    #[test]
    fn absent_fraction_flag_ignores_payload_bytes() {
        match ProtoEvent::try_from(enter_buf(Position::Right, 0, 0.75)).expect("valid") {
            ProtoEvent::Enter(_, f) => assert_eq!(f, None),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
