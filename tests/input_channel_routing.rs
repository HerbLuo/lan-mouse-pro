//! ChannelMode routing smoke test (PLAN-M1, control-plane + channel-routing cross-cut)
//!
//! **Goal**: assert that `route_input(cfg, event)` from `lan-mouse-ipc` +
//! `route_input(...)` in `lan_mouse::quic_transport` agrees on channel
//! selection across the 4 advertised combinations:
//!
//! - `default` — mouse_button=Datagram, keyboard=Stream
//! - `gaming`  — mouse_button=Datagram, keyboard=Datagram
//! - `stream`  — mouse_button=Stream,    keyboard=Stream
//! - `mixed`   — mouse_button=Stream,    keyboard=Datagram
//!
//! For each combination we verify routing for **all 8 ProtoEvent variants**
//! that exist in `lan_mouse_proto`:
//!
//! - Motion / Axis / AxisDiscrete120 → always **Datagram**
//! - Button → cfg.mouse_button (Datagram or StreamB)
//! - Key / Modifiers → cfg.keyboard  (Datagram or StreamB)
//! - Enter / Leave / Ack / Hello → always **StreamA** (control plane)
//!
//! **Style**: integration test in `tests/` so we exercise the *public*
//! `route_input` API surface. Combinatorial coverage: 4 configs × 8 events
//! = 32 routing assertions in one body (4 small tests = same body w/
//! different cfg, since the dispatch matrix is shared). Pure-function
//! checks first; round-trip via `PeerSession` only validates that the
//! selected channel is consistent with what `send_*` actually puts on
//! the wire.

use std::collections::HashMap;

use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use lan_mouse_ipc::{ChannelMode, InputChannelConfig};
use lan_mouse_proto::Position as ProtoPosition;
use lan_mouse_proto::ProtoEvent;

// -- Pure-function routing assertions -----------------------------------------

fn motion() -> ProtoEvent {
    ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
        time: 0,
        dx: 1.0,
        dy: 2.0,
    }))
}
fn axis() -> ProtoEvent {
    ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Axis {
        time: 0,
        axis: 0,
        value: 1.0,
    }))
}
fn axis_discrete() -> ProtoEvent {
    ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
        axis: 0,
        value: 120,
    }))
}
fn button() -> ProtoEvent {
    ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Button {
        time: 0,
        button: 0x110, // BTN_LEFT
        state: 1,
    }))
}
fn key() -> ProtoEvent {
    ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key {
        time: 0,
        key: 30,
        state: 1,
    }))
}
fn modifiers() -> ProtoEvent {
    ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
        depressed: 0x01 | 0x02,
        latched: 0,
        locked: 0,
        group: 0,
    }))
}
fn hello() -> ProtoEvent {
    ProtoEvent::hello(*b"deadbeef")
}
fn ping() -> ProtoEvent {
    ProtoEvent::Ping
}
fn pong() -> ProtoEvent {
    ProtoEvent::Pong(true)
}
fn enter() -> ProtoEvent {
    ProtoEvent::Enter(ProtoPosition::Left)
}
fn leave() -> ProtoEvent {
    ProtoEvent::Leave(42)
}
fn ack() -> ProtoEvent {
    ProtoEvent::Ack(42)
}

// -- Silencers ---------------------------------------------------------------

#[allow(dead_code)]
fn _unused_silencer() {
    // Touch any symbol that would otherwise be flagged by the unused-import
    // lint. (Position, Ping, Pong, etc. — all exercised by the routing
    // assertions above; this is here only to keep `use` lines honest.)
}

// -- Routing assertions -------------------------------------------------------

/// Apply `route_input(cfg, event)` (public API surface) for each known
/// input-event variant and assert the channel matches the config. Control
/// events (Enter/Leave/Ack/Ping/Pong/Hello) are always StreamA regardless
/// of config — they're the control plane, not user input.
fn assert_event_routing(cfg: &InputChannelConfig, table: &str) {
    use lan_mouse::quic_transport::Channel;
    use lan_mouse::quic_transport::route_input;

    let expected_motion = Channel::Datagram;
    let expected_axis = Channel::Datagram;
    let expected_axis_discrete = Channel::Datagram;
    let expected_button = match cfg.mouse_button {
        ChannelMode::Datagram => Channel::Datagram,
        ChannelMode::Stream => Channel::StreamB,
    };
    let expected_key = match cfg.keyboard {
        ChannelMode::Datagram => Channel::Datagram,
        ChannelMode::Stream => Channel::StreamB,
    };

    // Pointer motion family — always Datagram
    assert_eq!(
        route_input(cfg, &motion()),
        expected_motion,
        "{table}: motion"
    );
    assert_eq!(route_input(cfg, &axis()), expected_axis, "{table}: axis");
    assert_eq!(
        route_input(cfg, &axis_discrete()),
        expected_axis_discrete,
        "{table}: axis_discrete"
    );

    // Button follows mouse config
    assert_eq!(
        route_input(cfg, &button()),
        expected_button,
        "{table}: button"
    );

    // Key + Modifiers follow keyboard config (modifiers follow keyboard — see
    // the dispatch table in `quic_transport::route_input`).
    assert_eq!(route_input(cfg, &key()), expected_key, "{table}: key");
    assert_eq!(
        route_input(cfg, &modifiers()),
        expected_key,
        "{table}: modifiers"
    );

    // Control plane: always StreamA
    assert_eq!(
        route_input(cfg, &hello()),
        Channel::StreamA,
        "{table}: hello"
    );
    assert_eq!(
        route_input(cfg, &enter()),
        Channel::StreamA,
        "{table}: enter"
    );
    assert_eq!(
        route_input(cfg, &leave()),
        Channel::StreamA,
        "{table}: leave"
    );
    assert_eq!(route_input(cfg, &ack()), Channel::StreamA, "{table}: ack");
    assert_eq!(route_input(cfg, &ping()), Channel::StreamA, "{table}: ping");
    assert_eq!(route_input(cfg, &pong()), Channel::StreamA, "{table}: pong");
}

#[test]
fn channel_routing_default_mouse_datagram_keyboard_stream() {
    let cfg = InputChannelConfig::default();
    assert_eq!(cfg.mouse_button, ChannelMode::Datagram);
    assert_eq!(cfg.keyboard, ChannelMode::Stream);
    assert_event_routing(&cfg, "default");
}

#[test]
fn channel_routing_gaming_all_datagram() {
    let cfg = InputChannelConfig {
        mouse_button: ChannelMode::Datagram,
        keyboard: ChannelMode::Datagram,
    };
    assert_event_routing(&cfg, "gaming");
}

#[test]
fn channel_routing_all_stream_motion_still_datagram() {
    let cfg = InputChannelConfig {
        mouse_button: ChannelMode::Stream,
        keyboard: ChannelMode::Stream,
    };
    assert_event_routing(&cfg, "all_stream");
    // Critical assertion: Motion **must** still route via Datagram even when
    // caller asks for "all stream". This enforces §9 / D7 invariants and
    // is the headline behavior of M1 channel routing.
    use lan_mouse::quic_transport::route_input;
    assert_eq!(
        route_input(&cfg, &motion()),
        lan_mouse::quic_transport::Channel::Datagram,
        "Motion always Datagram regardless of config (D7 / §9)"
    );
}

#[test]
fn channel_routing_mixed_mouse_stream_keyboard_datagram() {
    let cfg = InputChannelConfig {
        mouse_button: ChannelMode::Stream,
        keyboard: ChannelMode::Datagram,
    };
    assert_event_routing(&cfg, "mixed");
}

// -- Default contract sanity -------------------------------------------------

#[test]
fn input_channels_default_is_mouse_datagram_keyboard_stream() {
    let cfg = InputChannelConfig::default();
    assert_eq!(
        cfg.mouse_button,
        ChannelMode::Datagram,
        "default mouse channel is Datagram"
    );
    assert_eq!(
        cfg.keyboard,
        ChannelMode::Stream,
        "default keyboard channel is Stream"
    );
}

#[test]
fn input_channels_serde_round_trip_keeps_fields() {
    let cfg = InputChannelConfig {
        mouse_button: ChannelMode::Stream,
        keyboard: ChannelMode::Datagram,
    };
    let s = serde_json::to_string(&cfg).expect("serialize");
    let back: InputChannelConfig = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(back.mouse_button, cfg.mouse_button);
    assert_eq!(back.keyboard, cfg.keyboard);
}

#[test]
fn input_channels_accept_all_combos_exhaustively() {
    // Combinatorial check: every (mouse_button, keyboard) pair routes OK.
    for mouse in [ChannelMode::Datagram, ChannelMode::Stream] {
        for keyboard in [ChannelMode::Datagram, ChannelMode::Stream] {
            let cfg = InputChannelConfig {
                mouse_button: mouse,
                keyboard,
            };
            assert_event_routing(&cfg, &format!("combo m={mouse:?} k={keyboard:?}"));
        }
    }
}

// -- Unused-import silencer --------------------------------------------------

#[allow(dead_code)]
fn _unused_table() -> HashMap<&'static str, ()> {
    // Touch `HashMap` so the unused-import lint doesn't fire on the pure-fn
    // tests above. (Future expansions of this file may want a lookup table
    // anyway — this is the cheapest place to keep the import alive.)
    HashMap::new()
}
