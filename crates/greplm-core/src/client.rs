//! Client for the greplm daemon.

#[cfg(unix)]
pub use unix_impl::Client;

#[cfg(not(unix))]
pub use stub_impl::Client;

// The daemon is built on Unix domain sockets and is unavailable on other
// platforms. This stub lets the CLI compile everywhere; `try_connect` always
// returns `None`, so callers transparently fall back to in-process queries.
#[cfg(not(unix))]
mod stub_impl {
    use std::path::Path;

    use crate::error::{Error, Result};
    use crate::proto::{Request, Response};

    /// A connected client to a running greplm daemon (unsupported on this platform).
    pub struct Client {
        _private: (),
    }

    impl Client {
        /// Always returns `None`: no daemon is available on this platform.
        pub fn try_connect(_socket: &Path) -> Option<Client> {
            None
        }

        /// Always errors: no daemon is available on this platform.
        pub fn request(&mut self, _req: &Request) -> Result<Response> {
            Err(Error::other(
                "greplm daemon is not supported on this platform",
            ))
        }

        /// Always errors: no daemon is available on this platform.
        pub fn request_routed(
            &mut self,
            _root: &std::path::Path,
            _req: &Request,
        ) -> Result<Response> {
            Err(Error::other(
                "greplm daemon is not supported on this platform",
            ))
        }
    }
}

#[cfg(unix)]
mod unix_impl {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    use crate::error::{Error, Result};
    use crate::proto::{Request, Response, RoutedRequest};

    /// A connected client to a running greplm daemon.
    pub struct Client {
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl Client {
        /// Connect to a daemon listening on `socket`. Returns `None` if no daemon
        /// is reachable (so callers can fall back to in-process queries).
        pub fn try_connect(socket: &Path) -> Option<Client> {
            let stream = UnixStream::connect(socket).ok()?;
            let reader = BufReader::new(stream.try_clone().ok()?);
            Some(Client {
                reader,
                writer: stream,
            })
        }

        /// Send a request to a per-project daemon and read the response.
        pub fn request(&mut self, req: &Request) -> Result<Response> {
            self.round_trip(req)
        }

        /// Send a request to the global multi-root daemon, addressed to a
        /// specific project `root`, and read the response.
        pub fn request_routed(&mut self, root: &Path, req: &Request) -> Result<Response> {
            let routed = RoutedRequest {
                root: root.to_path_buf(),
                req: req.clone(),
            };
            self.round_trip(&routed)
        }

        fn round_trip<T: serde::Serialize>(&mut self, value: &T) -> Result<Response> {
            let mut bytes = serde_json::to_vec(value)?;
            bytes.push(b'\n');
            self.writer.write_all(&bytes).map_err(Error::PlainIo)?;
            self.writer.flush().map_err(Error::PlainIo)?;
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).map_err(Error::PlainIo)?;
            if n == 0 {
                return Err(Error::other("daemon closed connection"));
            }
            let resp: Response = serde_json::from_str(line.trim())?;
            Ok(resp)
        }
    }
}
