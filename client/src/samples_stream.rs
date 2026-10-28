//! The writes of a samples stream. The request line and the header leave in one write as the stream
//! opens, and each tick's records leave in one write as soon as they are taken; nothing waits in a buffer.
//! The capture service reads the request line within its request timeout and takes a stream that sends no
//! byte for a minute as dead, so bytes held back by a buffer starve it whenever the sample rate is low: at
//! 1 Hz a 256 KB buffer holds the request line for minutes, the service answers `ERR reading the request`
//! and closes, and the session's samples are lost.

use mercs2_formats::profd::Request;
use std::io::Write;

/// The writing end of one samples stream.
pub struct Sender<W: Write> {
    w: W,
}

impl<W: Write> Sender<W> {
    pub fn new(w: W) -> Self {
        Sender { w }
    }

    /// Open the stream of `session`: its request line followed by the encoded samples header, in one write.
    pub fn open(&mut self, session: &str, header: &[u8]) -> Result<(), String> {
        let mut opening = Request::Samples { session: session.to_owned() }.line().into_bytes();
        opening.extend_from_slice(header);
        self.w.write_all(&opening).map_err(|e| format!("opening the samples stream: {e}"))
    }

    /// Send one tick's records.
    pub fn tick(&mut self, records: &[u8]) -> Result<(), String> {
        self.w.write_all(records).map_err(|e| format!("sending samples: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A writer that keeps every write it receives, as the socket sees them.
    #[derive(Default)]
    struct Socket {
        writes: Rc<RefCell<Vec<Vec<u8>>>>,
    }

    impl Write for Socket {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.borrow_mut().push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_request_line_and_header_reach_the_socket_as_the_stream_opens() {
        let socket = Socket::default();
        let writes = socket.writes.clone();
        let mut s = Sender::new(socket);
        s.open("20261026-221429", b"HEADER").unwrap();
        assert_eq!(writes.borrow().as_slice(), [b"SAMPLES 20261026-221429\nHEADER".to_vec()]);
    }

    #[test]
    fn every_tick_reaches_the_socket_as_it_is_taken() {
        let socket = Socket::default();
        let writes = socket.writes.clone();
        let mut s = Sender::new(socket);
        s.open("a", b"H").unwrap();
        s.tick(b"tick 1").unwrap();
        s.tick(b"tick 2").unwrap();
        assert_eq!(writes.borrow().as_slice(), [b"SAMPLES a\nH".to_vec(), b"tick 1".to_vec(), b"tick 2".to_vec()]);
    }

    #[test]
    fn a_closed_socket_fails_the_tick_that_hits_it() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut s = Sender::new(Closed);
        assert!(s.open("a", b"H").unwrap_err().starts_with("opening the samples stream: "));
        assert!(s.tick(b"t").unwrap_err().starts_with("sending samples: "));
    }
}
