pub mod frame;
pub use frame::{
    split_payload, Frame, FrameType, MessageReassembler, StreamChunk, MAGIC, MAX_PAYLOAD_SIZE,
};

#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "host")]
pub mod security;

#[cfg(feature = "guest")]
pub mod guest;

#[cfg(feature = "guest")]
pub use guest::{
    guest_dial, guest_dial_with, handle_guest_connection, run_guest_registration, GuestListener,
    GuestTransport,
};
#[cfg(feature = "host")]
pub use host::{handle_registration_handshake, host_dial, host_ping, HostListener};
#[cfg(feature = "host")]
pub use security::{
    AttestationVerifier, ChallengeRegistry, DevChallengeOnlyVerifier, Ed25519AttestationVerifier,
};
