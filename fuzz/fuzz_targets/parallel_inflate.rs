//! libFuzzer target: `ParallelInflater` must agree with zlib-rs on any input.
//!
//! Input layout: byte 0 = threads (low 3 bits + 1) and mode (bit 7: the rest is plain data that is first compressed
//! with zlib-rs at level `(byte 0 >> 3) & 0xf` (clamped to 9), with a sync flush every `byte 3` bytes when nonzero,
//! so that valid streams with many blocks are covered too; otherwise the rest is the raw deflate stream), bytes 1-2 =
//! chunk size in bytes (1..=65536, small sizes force many chunk boundaries), byte 3 = flush interval, rest = payload.
//!
//! Checks: if zlib-rs decodes the stream, the output is identical; if it rejects it, `ParallelInflater` fails too,
//! everything it returned first is a prefix of zlib-rs's partial output, and later reads keep failing. Panics
//! (including in worker threads, via the panic hook) and hangs are reported by libFuzzer.
#![no_main]

use std::io::{Read, Write};
use std::sync::{Arc, Once};

use libfuzzer_sys::fuzz_target;
use planetiler_core::util::parallel_inflate::ParallelInflater;

/// Skip inputs whose output would be larger than this (a 64 KB input can expand to ~64 MB).
const MAX_OUTPUT: usize = 256 << 20;

/// zlib-rs: `Some(Ok(output))`, `Some(Err(partial output))`, or `None` if the output is too large to compare.
fn reference(data: &[u8]) -> Option<Result<Vec<u8>, Vec<u8>>> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut d = Decompress::new(false);
    let mut out = Vec::with_capacity(1 << 16);
    loop {
        if out.len() > MAX_OUTPUT {
            return None;
        }
        if out.capacity() - out.len() < 1 << 16 {
            out.reserve(out.len().max(1 << 16));
        }
        let (i, o) = (d.total_in(), d.total_out());
        match d.decompress_vec(&data[i as usize..], &mut out, FlushDecompress::None) {
            Ok(Status::StreamEnd) => return Some(Ok(out)),
            Ok(_) if d.total_in() == i && d.total_out() == o => return Some(Err(out)),
            Ok(_) => {}
            Err(_) => return Some(Err(out)),
        }
    }
}

fn compress(data: &[u8], level: u32, flush_every: usize) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level.min(9)));
    if flush_every == 0 {
        e.write_all(data).unwrap();
    } else {
        for piece in data.chunks(flush_every) {
            e.write_all(piece).unwrap();
            e.flush().unwrap();
        }
    }
    e.finish().unwrap()
}

fuzz_target!(|input: &[u8]| {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        // the inflater turns worker panics into fallbacks; make them crash the fuzzer instead
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default(info);
            std::process::abort();
        }));
    });
    if input.len() < 4 {
        return;
    }
    let threads = 1 + (input[0] & 7) as usize;
    let chunk = 1 + u16::from_le_bytes([input[1], input[2]]) as usize;
    let payload = &input[4..];
    let stream = if input[0] & 0x80 != 0 {
        compress(payload, ((input[0] >> 3) & 0xf) as u32, input[3] as usize)
    } else {
        payload.to_vec()
    };
    let Some(expected) = reference(&stream) else { return };

    let mut inflater = ParallelInflater::with_any_chunk_size(Arc::new(stream), threads, chunk);
    let mut out = vec![];
    let mut buf = vec![0u8; 1 + (input[3] as usize) * 37];
    let error = loop {
        match inflater.read(&mut buf) {
            Ok(0) => break None,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => break Some(e),
        }
        assert!(out.len() <= MAX_OUTPUT + (8 << 20), "output much larger than zlib-rs's");
    };
    match (&expected, error) {
        (Ok(reference), None) => assert!(out == *reference, "output differs from zlib-rs"),
        (Ok(_), Some(e)) => panic!("rejected a stream zlib-rs accepts: {e}"),
        (Err(_), None) => panic!("accepted a stream zlib-rs rejects"),
        (Err(partial), Some(_)) => {
            assert!(partial.starts_with(&out), "output before the error is not a prefix of zlib-rs's");
            assert!(inflater.read(&mut buf).is_err(), "errors must be sticky");
        }
    }
});
