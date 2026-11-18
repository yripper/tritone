//! `compress=zlib-stream`: one zlib context for the whole connection. Frames
//! are buffered until one ends in `00 00 ff ff`; that marks a complete message.
//! https://docs.discord.food/gateway/using-gateway#transport-compression

use anyhow::{Result, bail};
use flate2::{Decompress, FlushDecompress, Status};

use crate::discord::transport::WS_MAX_SIZE;

pub(crate) const SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// One per connection: a resumed or new socket starts a fresh one.
pub(crate) struct Inflater {
    z: Decompress,
    buf: Vec<u8>,
    limit: usize,
}

impl Inflater {
    pub(crate) fn new() -> Inflater {
        Inflater::with_limit(WS_MAX_SIZE)
    }

    pub(crate) fn with_limit(limit: usize) -> Inflater {
        Inflater { z: Decompress::new(true), buf: Vec::new(), limit }
    }

    /// Feeds one WebSocket frame. Returns the decompressed message once the
    /// buffered frames complete one. Errors (corrupt stream, over the cap)
    /// mean the connection must be dropped: the zlib context is unusable.
    pub(crate) fn push(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.buf.len() + frame.len() > self.limit {
            bail!("gateway message over {} bytes", self.limit);
        }
        self.buf.extend_from_slice(frame);
        if !self.buf.ends_with(&SUFFIX) {
            return Ok(None);
        }
        let input = std::mem::take(&mut self.buf);
        let mut out = Vec::with_capacity(input.len() * 4);
        let mut pos = 0;
        loop {
            let before_in = self.z.total_in();
            let status = self.z.decompress_vec(&input[pos..], &mut out, FlushDecompress::Sync)?;
            pos += (self.z.total_in() - before_in) as usize;
            if out.len() > self.limit {
                bail!("gateway message over {} bytes", self.limit);
            }
            match status {
                Status::StreamEnd => break,
                _ if pos >= input.len() && out.len() < out.capacity() => break,
                _ => out.reserve(out.capacity().max(64 << 10)),
            }
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
#[path = "zlib_tests.rs"]
mod tests;
