//! Transport-independent implementation of Git's `connect` remote helper.

use std::io::Read;
use std::io::Write;
use std::thread;

use reportify::Report;

use crate::GitError;
use crate::GitResult;

const MAX_HELPER_COMMAND_BYTES: usize = 1024;

/// Git smart-protocol service requested through a remote helper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteService {
    /// Read-only fetch and clone service.
    UploadPack,
    /// Ref-update and object-ingress service.
    ReceivePack,
}

/// Runs Git's `connect` remote-helper handshake over supplied blocking I/O.
///
/// `connect` decides which services are permitted and returns independent
/// reader and writer halves for the selected service. Tascarrel adapters can
/// connect those halves to Unix sockets or other blocking bridges without
/// coupling this crate to a daemon or mux protocol.
///
/// # Errors
///
/// Returns a report for malformed helper commands, a rejected service, or a
/// failed byte relay.
pub fn run_remote_helper<I, O, R, W, C>(mut input: I, mut output: O, connect: C) -> GitResult<()>
where
    I: Read + Send + 'static,
    O: Write,
    R: Read,
    W: Write + Send + 'static,
    C: FnOnce(RemoteService) -> GitResult<(R, W)>,
{
    if read_command(&mut input)? != b"capabilities" {
        return Err(Report::new(GitError::UnsupportedService {
            service: "remote-helper command before capabilities".to_owned(),
        }));
    }
    output.write_all(b"connect\n\n").map_err(helper_io)?;
    output.flush().map_err(helper_io)?;
    let command = read_command(&mut input)?;
    let service = match command.as_slice() {
        b"connect git-upload-pack" => RemoteService::UploadPack,
        b"connect git-receive-pack" => RemoteService::ReceivePack,
        _ => {
            return Err(Report::new(GitError::UnsupportedService {
                service: String::from_utf8_lossy(&command).into_owned(),
            }));
        }
    };
    let (mut service_reader, mut service_writer) = connect(service)?;
    output.write_all(b"\n").map_err(helper_io)?;
    output.flush().map_err(helper_io)?;

    let to_service = thread::spawn(move || copy_stream(&mut input, &mut service_writer));
    copy_stream(&mut service_reader, &mut output).map_err(helper_io)?;
    output.flush().map_err(helper_io)?;
    // The service closing its output is the completion signal for a smart
    // protocol exchange. Git keeps the helper's stdin open until the helper
    // exits, so joining the client-to-service relay here would deadlock with
    // Git waiting for us. The helper executable exits immediately after this
    // function returns, which terminates the blocked relay thread and closes
    // the service's write half.
    drop(to_service);
    Ok(())
}

fn copy_stream(reader: &mut impl Read, writer: &mut impl Write) -> std::io::Result<()> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return writer.flush();
        }
        writer.write_all(&buffer[..read])?;
        writer.flush()?;
    }
}

fn read_command(input: &mut impl Read) -> GitResult<Vec<u8>> {
    let mut command = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        input.read_exact(&mut byte).map_err(helper_io)?;
        if byte[0] == b'\n' {
            return Ok(command);
        }
        if command.len() == MAX_HELPER_COMMAND_BYTES {
            return Err(Report::new(GitError::UnsupportedService {
                service: "remote-helper command exceeds its bound".to_owned(),
            }));
        }
        command.push(byte[0]);
    }
}

fn helper_io(source: std::io::Error) -> Report<GitError> {
    Report::new(GitError::Io {
        action: "relay the Git remote helper",
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::Condvar;
    use std::sync::Mutex;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    struct SharedWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        wrote: mpsc::Sender<()>,
    }

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            let _ = self.wrote.send(());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct BlockingAfterInput {
        input: Cursor<Vec<u8>>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Read for BlockingAfterInput {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let read = self.input.read(bytes)?;
            if read != 0 {
                return Ok(read);
            }
            let (released, ready) = &*self.release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
            Ok(0)
        }
    }

    /// Verifies upload-pack negotiation and payloads are relayed in both
    /// directions.
    #[test]
    fn relays_the_connect_remote_helper_protocol() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let service_written = Arc::clone(&written);
        let (wrote, written_request) = mpsc::channel();
        let input = Cursor::new(b"capabilities\nconnect git-upload-pack\nrequest".to_vec());
        let mut output = Vec::new();

        run_remote_helper(input, &mut output, move |service| {
            assert_eq!(service, RemoteService::UploadPack);
            Ok((
                Cursor::new(b"response".to_vec()),
                SharedWriter {
                    bytes: service_written,
                    wrote,
                },
            ))
        })
        .unwrap();

        written_request
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(output, b"connect\n\n\nresponse");
        assert_eq!(&*written.lock().unwrap(), b"request");
    }

    /// Verifies service EOF completes the helper even while Git retains its
    /// stdin until the helper exits.
    #[test]
    fn service_eof_does_not_wait_for_remote_stdin_eof() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let input = BlockingAfterInput {
            input: Cursor::new(b"capabilities\nconnect git-upload-pack\nrequest".to_vec()),
            release: Arc::clone(&release),
        };
        let (completed, completion) = mpsc::channel();
        let helper = thread::spawn(move || {
            let result = run_remote_helper(input, Vec::new(), |_| {
                Ok((Cursor::new(b"response".to_vec()), std::io::sink()))
            });
            let _ = completed.send(result);
        });

        let result = completion.recv_timeout(Duration::from_secs(1));
        let (released, ready) = &*release;
        *released.lock().unwrap() = true;
        ready.notify_all();
        helper.join().unwrap();
        result.unwrap().unwrap();
    }
}
