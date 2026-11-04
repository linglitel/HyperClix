//! The same upstream KCP implementation used by the Python reference.

use crate::Result;
use std::{
    ffi::{c_char, c_int, c_long, c_void},
    ptr::NonNull,
};

unsafe extern "C" {
    fn ikcp_create(conv: u32, user: *mut c_void) -> *mut c_void;
    fn ikcp_release(kcp: *mut c_void);
    fn ikcp_setoutput(
        kcp: *mut c_void,
        callback: unsafe extern "C" fn(*const c_char, c_int, *mut c_void, *mut c_void) -> c_int,
    );
    fn ikcp_send(kcp: *mut c_void, data: *const c_char, len: c_int) -> c_int;
    fn ikcp_input(kcp: *mut c_void, data: *const c_char, len: c_long) -> c_int;
    fn ikcp_update(kcp: *mut c_void, current: u32);
    fn ikcp_flush(kcp: *mut c_void);
    fn ikcp_peeksize(kcp: *const c_void) -> c_int;
    fn ikcp_recv(kcp: *mut c_void, buffer: *mut c_char, len: c_int) -> c_int;
    fn ikcp_wndsize(kcp: *mut c_void, snd: c_int, rcv: c_int) -> c_int;
    fn ikcp_nodelay(
        kcp: *mut c_void,
        nodelay: c_int,
        interval: c_int,
        resend: c_int,
        nc: c_int,
    ) -> c_int;
}

pub(crate) struct Kcp {
    handle: NonNull<c_void>,
    output: Box<Vec<Vec<u8>>>,
}

// A Kcp instance belongs to one async actor. C never retains a thread-local pointer.
unsafe impl Send for Kcp {}

unsafe extern "C" fn emit(
    data: *const c_char,
    len: c_int,
    _: *mut c_void,
    user: *mut c_void,
) -> c_int {
    unsafe {
        let queue = &mut *(user as *mut Vec<Vec<u8>>);
        queue.push(std::slice::from_raw_parts(data.cast::<u8>(), len as usize).to_vec());
    }
    0
}

impl Kcp {
    pub fn new() -> Result<Self> {
        let mut output = Box::<Vec<Vec<u8>>>::default();
        let handle = unsafe {
            NonNull::new(ikcp_create(
                0x12345678,
                (&mut *output as *mut Vec<Vec<u8>>).cast(),
            ))
        }
        .ok_or_else(|| anyhow::anyhow!("KCP allocation failed"))?;
        unsafe {
            ikcp_setoutput(handle.as_ptr(), emit);
            ikcp_wndsize(handle.as_ptr(), 4096, 4096);
            ikcp_nodelay(handle.as_ptr(), 1, 10, 2, 1);
        }
        Ok(Self { handle, output })
    }

    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        let result = unsafe {
            ikcp_send(
                self.handle.as_ptr(),
                data.as_ptr().cast(),
                data.len() as c_int,
            )
        };
        anyhow::ensure!(result >= 0, "KCP send failed: {result}");
        Ok(())
    }

    pub fn input(&mut self, packet: &[u8]) -> Result<()> {
        let result = unsafe {
            ikcp_input(
                self.handle.as_ptr(),
                packet.as_ptr().cast(),
                packet.len() as c_long,
            )
        };
        anyhow::ensure!(result >= 0, "KCP rejected datagram: {result}");
        Ok(())
    }

    pub fn update(&mut self, current: u32) {
        unsafe { ikcp_update(self.handle.as_ptr(), current) }
    }
    pub fn flush(&mut self) {
        unsafe { ikcp_flush(self.handle.as_ptr()) }
    }
    pub fn output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.output)
    }
    pub fn receive(&mut self) -> Result<Option<Vec<u8>>> {
        let length = unsafe { ikcp_peeksize(self.handle.as_ptr()) };
        if length < 0 {
            return Ok(None);
        }
        let mut buffer = vec![0; length as usize];
        let count = unsafe { ikcp_recv(self.handle.as_ptr(), buffer.as_mut_ptr().cast(), length) };
        anyhow::ensure!(count >= 0, "KCP receive failed: {count}");
        buffer.truncate(count as usize);
        Ok(Some(buffer))
    }
}

impl Drop for Kcp {
    fn drop(&mut self) {
        unsafe { ikcp_release(self.handle.as_ptr()) }
    }
}
