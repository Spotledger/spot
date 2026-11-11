#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The terminal seam: the fake drives a scripted session and captures output.

use spot_break_glass::{FakePty, Pty};

#[test]
fn the_fake_terminal_scripts_input_and_captures_output() {
    let mut pty = FakePty::new(b"ls -la\n");
    let mut buf = [0u8; 4];
    let read = pty.read(&mut buf).unwrap();
    assert_eq!(read, 4);
    assert_eq!(&buf, b"ls -");
    pty.write(b"output\n").unwrap();
    pty.resize(120, 40).unwrap();
    assert_eq!(pty.size(), (120, 40));
    assert_eq!(pty.output(), b"output\n");
    assert!(!pty.at_end());

    let mut rest = [0u8; 16];
    let read = pty.read(&mut rest).unwrap();
    assert_eq!(&rest[..read], b"la\n");
    assert!(pty.at_end());
    assert_eq!(pty.read(&mut rest).unwrap(), 0, "closed at end of input");
}
