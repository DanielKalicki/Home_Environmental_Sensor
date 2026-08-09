//! Permanent buffers carved out of the external PSRAM.
//!
//! The XIAO carries an ESP32-S3R8 module, whose 8 MB of octal-SPI PSRAM are
//! mapped into the data address space by [`esp_hal::psram::init_psram`]. That
//! call only makes the memory *reachable*; it does not hand out any storage.
//! [`Psram`] does that: it is a bump allocator that gives away the mapped
//! region from the bottom up.
//!
//! Only permanent buffers are allocated, so nothing is ever freed and no free
//! list is needed. Every allocation therefore lives for the whole program run
//! and can be handed out as a `&'static mut` slice. In exchange the firmware
//! keeps its internal RAM — which the Wi-Fi driver and the network stack
//! compete for — untouched.

use core::mem::{align_of, size_of};

use esp_hal::peripheral::Peripheral;
use esp_hal::peripherals::PSRAM;
use esp_hal::psram;

/// Bump allocator over the mapped PSRAM window.
pub struct Psram {
    /// Address the next allocation starts at, before alignment.
    next: usize,
    /// First address past the mapped window.
    end: usize,
}

impl Psram {
    /// Map the PSRAM and take ownership of the whole mapped window.
    ///
    /// This reconfigures the data cache, so it must run before the Wi-Fi
    /// driver and the sensor tasks are started.
    pub fn init(peripheral: impl Peripheral<P = PSRAM>) -> Self {
        psram::init_psram(peripheral);

        let start = psram::psram_vaddr_start();
        Self {
            next: start,
            end: start + psram::PSRAM_BYTES,
        }
    }

    /// Bytes still available for further allocations.
    pub const fn free_bytes(&self) -> usize {
        self.end - self.next
    }

    /// Reserve `len` elements of `T` in PSRAM, each set to `value`.
    ///
    /// Returns `None` if the remaining window is too small, so the caller can
    /// report a configuration problem instead of faulting on a stray address.
    pub fn alloc_slice<T: Copy>(&mut self, len: usize, value: T) -> Option<&'static mut [T]> {
        let align = align_of::<T>();
        let start = self.next.checked_add(align - 1)? & !(align - 1);
        let end = start.checked_add(size_of::<T>().checked_mul(len)?)?;

        if end > self.end {
            return None;
        }
        self.next = end;

        // SAFETY: `start .. end` lies inside the window mapped by
        // `init_psram`, is aligned for `T`, and is handed out exactly once
        // because `self.next` only ever moves forward. Every element is
        // written before the slice is formed, so none of it is left
        // uninitialised.
        Some(unsafe {
            let pointer = start as *mut T;
            for index in 0..len {
                pointer.add(index).write(value);
            }
            core::slice::from_raw_parts_mut(pointer, len)
        })
    }
}
