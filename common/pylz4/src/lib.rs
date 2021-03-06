// Copyright (c) 2018-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

// Support for lz4revlog

#![deny(warnings)]

extern crate byteorder;
#[macro_use]
extern crate failure_ext as failure;
extern crate lz4;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use failure::Error;
use std::io::Cursor;
use std::ptr;

use lz4::liblz4::{LZ4F_compressBound, LZ4StreamDecode, LZ4_compress_continue, LZ4_createStream,
                  LZ4_createStreamDecode, LZ4_decompress_safe_continue, LZ4_freeStreamDecode};

#[derive(Debug, Fail)]
pub enum ErrorKind {
    #[fail(display = "Bad LZ4: {}", _0)] BadLZ4(String),
    #[fail(display = "Failed to init LZ4 context")] LZ4InitFailed,
    #[fail(display = "Compression failed")] LZ4CompressFailed,
}

// Wrapper for the lz4 library context
struct Context(*mut LZ4StreamDecode);
impl Context {
    // Allocate a context; fails if allocation fails
    fn new() -> Result<Self, &'static str> {
        let ctx = unsafe { LZ4_createStreamDecode() };
        if ctx.is_null() {
            Err("failed to create LZ4 context")
        } else {
            Ok(Context(ctx))
        }
    }
}

// Make sure C resources for context get freed.
impl Drop for Context {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LZ4_freeStreamDecode(self.0) };
            self.0 = ptr::null_mut();
        }
    }
}

// Decompress a raw lz4 block
fn decompress_block(i: &[u8], out: &mut Vec<u8>) -> Result<usize, String> {
    let ctx = Context::new()?;
    unsafe {
        let ret = LZ4_decompress_safe_continue(
            ctx.0,
            i.as_ptr(),
            out.as_mut_ptr(),
            i.len() as i32,
            out.capacity() as i32,
        );
        if ret < 0 {
            Err("LZ4_decompress_safe_continue failed".into())
        } else {
            out.set_len(ret as usize);
            Ok(ret as usize)
        }
    }
}

// Decompresses lz4 and returns decompressed data and pointer to the unused data
pub fn decompress(i: &[u8]) -> Result<(Vec<u8>, &[u8]), Error> {
    let mut input = Cursor::new(i);
    let origsize = input.read_u32::<LittleEndian>()?;
    let input = &i[4..];
    let mut data = Vec::with_capacity(origsize as usize);

    match decompress_block(input, &mut data) {
        Ok(len) => {
            assert_eq!(origsize as usize, len);
            assert_eq!(origsize as usize, data.len());
        }
        Err(msg) => bail_err!(ErrorKind::BadLZ4(msg)),
    };

    let inused = i.len();
    let remains = &i[inused..];
    Ok((data, remains))
}

pub fn compress(input_data: &[u8]) -> Result<Vec<u8>, Error> {
    let ctx = unsafe { LZ4_createStream() };
    if ctx.is_null() {
        bail_err!(ErrorKind::LZ4InitFailed);
    }
    // First 4 bytes is an original size stored as le32
    let prefix = 4;
    let mut compressed =
        Vec::with_capacity(prefix + unsafe { LZ4F_compressBound(input_data.len(), ptr::null()) });

    compressed
        .write_u32::<LittleEndian>(input_data.len() as u32)
        .unwrap();
    unsafe {
        let dest = compressed.as_mut_ptr().offset(4);
        let res = LZ4_compress_continue(ctx, input_data.as_ptr(), dest, input_data.len() as i32);
        if res == 0 {
            bail_err!(ErrorKind::LZ4CompressFailed);
        }
        compressed.set_len((res + 4) as usize);
    }
    Ok(compressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_compress_decompress() {
        let data = "testdata".as_bytes();
        let compressed = compress(data).unwrap();
        let (res, remains) = decompress(&compressed).unwrap();
        assert!(remains.is_empty());
        assert_eq!(data, res.as_slice());
    }
}
