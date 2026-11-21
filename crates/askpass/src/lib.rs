//! The wire protocol between the askpass helper and the shell, and the
//! helper's client side. std only.
//!
//! One request per connection:
//!
//! - request: `u16` token length, token, `u32` prompt length, prompt (all
//!   big endian, text as UTF-8)
//! - reply: one status byte (`0` answer, `1` cancelled), then for an answer
//!   `u32` length and the answer
//!
//! The token (from the environment, set by the shell per operation) keeps
//! another local process from asking the app to show a prompt or from
//! stealing the answer.

use std::io::{self, Read, Write};

/// Environment variable holding the pipe name (Windows) or socket path.
pub const ENV_ADDRESS: &str = "INVOKE_GIT_ASKPASS_ADDR";
/// Environment variable holding the per-operation token.
pub const ENV_TOKEN: &str = "INVOKE_GIT_ASKPASS_TOKEN";

const MAX_TOKEN: usize = 256;
const MAX_TEXT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub token: String,
    pub prompt: String,
}

/// `None` is a cancel.
pub type Reply = Option<String>;

pub fn write_request(out: &mut impl Write, request: &Request) -> io::Result<()> {
    let token = request.token.as_bytes();
    let prompt = request.prompt.as_bytes();
    let token_len = u16::try_from(token.len()).map_err(|_| too_long())?;
    let prompt_len = u32::try_from(prompt.len()).map_err(|_| too_long())?;
    let mut bytes = Vec::with_capacity(6 + token.len() + prompt.len());
    bytes.extend_from_slice(&token_len.to_be_bytes());
    bytes.extend_from_slice(token);
    bytes.extend_from_slice(&prompt_len.to_be_bytes());
    bytes.extend_from_slice(prompt);
    out.write_all(&bytes)?;
    out.flush()
}

pub fn read_request(input: &mut impl Read) -> io::Result<Request> {
    let token_len = usize::from(read_u16(input)?);
    if token_len > MAX_TOKEN {
        return Err(too_long());
    }
    let token = read_text(input, token_len)?;
    let prompt_len = read_u32(input)? as usize;
    if prompt_len > MAX_TEXT {
        return Err(too_long());
    }
    let prompt = read_text(input, prompt_len)?;
    Ok(Request { token, prompt })
}

pub fn write_reply(out: &mut impl Write, reply: &Reply) -> io::Result<()> {
    let mut bytes = Vec::new();
    match reply {
        Some(answer) => {
            let len = u32::try_from(answer.len()).map_err(|_| too_long())?;
            bytes.push(0);
            bytes.extend_from_slice(&len.to_be_bytes());
            bytes.extend_from_slice(answer.as_bytes());
        }
        None => bytes.push(1),
    }
    out.write_all(&bytes)?;
    out.flush()
}

pub fn read_reply(input: &mut impl Read) -> io::Result<Reply> {
    let mut status = [0u8; 1];
    input.read_exact(&mut status)?;
    match status[0] {
        0 => {
            let len = read_u32(input)? as usize;
            if len > MAX_TEXT {
                return Err(too_long());
            }
            Ok(Some(read_text(input, len)?))
        }
        1 => Ok(None),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad status")),
    }
}

fn read_u16(input: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    input.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_text(input: &mut impl Read, len: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; len];
    input.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

fn too_long() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "message too long")
}

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = std::fs::File;

#[cfg(unix)]
fn connect(address: &str) -> io::Result<Stream> {
    Stream::connect(address)
}

/// Opens the named pipe like a file. All instances can be busy for a moment
/// while the server prepares the next one, so retry briefly.
#[cfg(windows)]
fn connect(address: &str) -> io::Result<Stream> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut tries = 0;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(address)
        {
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// Asks the app one question. `Ok(None)` means the user cancelled.
pub fn ask(address: &str, token: &str, prompt: &str) -> io::Result<Reply> {
    let mut stream = connect(address)?;
    write_request(
        &mut stream,
        &Request {
            token: token.to_owned(),
            prompt: prompt.to_owned(),
        },
    )?;
    read_reply(&mut stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn requests_round_trip(token in "\\PC{0,40}", prompt in "\\PC{0,200}") {
            let request = Request { token, prompt };
            let mut bytes = Vec::new();
            write_request(&mut bytes, &request).unwrap();
            prop_assert_eq!(read_request(&mut bytes.as_slice()).unwrap(), request);
        }

        #[test]
        fn replies_round_trip(answer in proptest::option::of("\\PC{0,200}")) {
            let mut bytes = Vec::new();
            write_reply(&mut bytes, &answer).unwrap();
            prop_assert_eq!(read_reply(&mut bytes.as_slice()).unwrap(), answer);
        }

        /// Garbage never panics and never allocates past the limits.
        #[test]
        fn garbage_is_an_error_not_a_panic(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = read_request(&mut bytes.as_slice());
            let _ = read_reply(&mut bytes.as_slice());
        }
    }

    #[test]
    fn oversized_prompts_are_refused() {
        let mut bytes = vec![0, 0];
        bytes.extend_from_slice(&(u32::MAX).to_be_bytes());
        assert!(read_request(&mut bytes.as_slice()).is_err());
    }
}
