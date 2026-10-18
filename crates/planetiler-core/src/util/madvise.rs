//! Port of `util/Madvise.java`: wrapper for the native `posix_madvise` function.
//!
//! Java needs JNR-FFI to call this; in Rust it is a direct `libc` call. Use the public API in
//! [`crate::util::byte_buffer_util::posix_madvise`].
//!
//! See [madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html).

use std::io;
use std::sync::LazyLock;

/// System page size in bytes, or `-1` if unavailable.
pub static PAGE_SIZE: LazyLock<i64> = LazyLock::new(|| {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size > 0 { size as i64 } else { -1 }
    }
    #[cfg(not(unix))]
    {
        -1
    }
});

fn aligned_address(address: usize, page_size: usize) -> usize {
    address & !(page_size - 1)
}

fn aligned_size(address: usize, capacity: usize, page_size: usize) -> usize {
    let end = address + capacity;
    let end = (end + page_size - 1) & !(page_size - 1);
    end - aligned_address(address, page_size)
}

/// Give a hint to the system how a memory region starting at `ptr` of `len` bytes will be used, where `advice` is
/// one of the `POSIX_MADV_*` constants. The region is expanded to page boundaries.
///
/// # Safety
///
/// `ptr..ptr+len` must lie within a single live memory mapping (e.g. a `memmap2::Mmap`).
pub unsafe fn posix_madvise(ptr: *const u8, len: usize, advice: i32) -> io::Result<()> {
    let page_size = *PAGE_SIZE;
    if page_size <= 0 {
        return Err(io::Error::other("madvise failed, pagesize not available"));
    }
    let page_size = page_size as usize;
    let address = ptr as usize;
    let start = aligned_address(address, page_size);
    let size = aligned_size(start, len + (address - start), page_size);
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    {
        // SAFETY: the caller guarantees the region is mapped; page-aligned expansion stays within the mapping's pages.
        let result = unsafe { libc::posix_madvise(start as *mut libc::c_void, size, advice) };
        if result != 0 {
            return Err(io::Error::other(format!("System call madvise failed with code: {result}")));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")))]
    {
        let _ = (size, advice);
        Err(io::Error::other("madvise not available on this system"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alignment() {
        assert_eq!(4096, aligned_address(4097, 4096));
        assert_eq!(4096, aligned_size(4096, 1, 4096));
        assert_eq!(8192, aligned_size(4096, 4097, 4096));
        assert!(*PAGE_SIZE > 0 || cfg!(not(unix)));
    }
}
