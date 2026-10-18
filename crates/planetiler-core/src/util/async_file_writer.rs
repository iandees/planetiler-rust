//! A buffered file writer that hands each full buffer to a background thread to write.

use std::fs::File;
use std::io::{self, Write};

/// Buffers writes and hands each full buffer to a background thread that writes it to the file, so the writing
/// thread does not stall in `write` syscalls (copying into the page cache, which can block when the system is
/// writing back a lot of dirty data). In OSM pass 2 the single feature writer thread spent ~2/3 of its time there,
/// and processing threads blocked on its full queue; in pass 1 the single node location writer spent ~15% there.
pub struct AsyncFileWriter {
    buf: Vec<u8>,
    capacity: usize,
    tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
    recycle: crossbeam_channel::Receiver<Vec<u8>>,
    handle: Option<std::thread::JoinHandle<io::Result<()>>>,
}

impl AsyncFileWriter {
    /// Buffers kept in flight between the writer and its I/O thread.
    const IN_FLIGHT: usize = 4;

    /// Starts an I/O thread named `thread_name` writing to `file` in buffers of `capacity` bytes.
    pub fn new(mut file: File, capacity: usize, thread_name: &str) -> io::Result<AsyncFileWriter> {
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(Self::IN_FLIGHT);
        let (recycle_tx, recycle) = crossbeam_channel::bounded::<Vec<u8>>(Self::IN_FLIGHT + 1);
        let handle = std::thread::Builder::new().name(thread_name.to_string()).spawn(move || -> io::Result<()> {
            for mut buf in rx {
                file.write_all(&buf)?;
                buf.clear();
                let _ = recycle_tx.try_send(buf);
            }
            file.flush()
        })?;
        let capacity = capacity.max(1);
        Ok(AsyncFileWriter { buf: Vec::with_capacity(capacity), capacity, tx: Some(tx), recycle, handle: Some(handle) })
    }

    fn send_buffer(&mut self) -> io::Result<()> {
        let capacity = self.capacity;
        let next = self.recycle.try_recv().unwrap_or_else(|_| Vec::with_capacity(capacity));
        let full = std::mem::replace(&mut self.buf, next);
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(full).is_ok());
        if sent { Ok(()) } else { Err(self.join().err().unwrap_or_else(|| io::Error::other("file writer stopped"))) }
    }

    /// Stops the I/O thread and returns its result.
    fn join(&mut self) -> io::Result<()> {
        self.tx = None;
        match self.handle.take() {
            Some(handle) => handle.join().map_err(|_| io::Error::other("file writer panicked"))?,
            None => Ok(()),
        }
    }

    /// Writes the remaining buffered bytes and waits for everything to be written.
    pub fn finish(mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            self.send_buffer()?;
        }
        self.join()
    }
}

impl Write for AsyncFileWriter {
    #[inline]
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.write_all(data)?;
        Ok(data.len())
    }

    #[inline]
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if self.buf.len() + data.len() > self.capacity && !self.buf.is_empty() {
            self.send_buffer()?;
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for AsyncFileWriter {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_everything_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let mut writer = AsyncFileWriter::new(File::create(&path).unwrap(), 100, "test-io").unwrap();
        let mut expected = Vec::new();
        for i in 0..10_000u32 {
            let bytes = i.to_le_bytes();
            writer.write_all(&bytes[..(i % 4 + 1) as usize]).unwrap();
            expected.extend_from_slice(&bytes[..(i % 4 + 1) as usize]);
        }
        writer.write_all(&[7; 1000]).unwrap();
        expected.extend_from_slice(&[7; 1000]);
        writer.finish().unwrap();
        assert_eq!(expected, std::fs::read(&path).unwrap());
    }
}
