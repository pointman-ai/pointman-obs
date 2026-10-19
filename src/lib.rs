//! OBS for Pointman: a recording watchdog. While OBS records, it holds each screen capture's picture
//! (obs-websocket's GetSourceScreenshot) against the real screen, and says so in a thread when the
//! picture stops while the screen moves. main.rs runs it under the node; the parts are here so the
//! tests can drive them.

pub mod frames;
pub mod obs;
pub mod screen;
pub mod sources;
pub mod watch;
pub mod words;
