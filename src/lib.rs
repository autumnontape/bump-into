#![no_std]

mod size_align;

use core::cell::UnsafeCell;
use core::fmt;
use core::mem::{self, MaybeUninit};
use core::ptr;

pub use size_align::{AlignOf, SizeOf};

/// A bump allocator over an arbitrary region of memory.
pub struct BumpInto<'a> {
    array: UnsafeCell<&'a mut [MaybeUninit<u8>]>,
}

impl<'this, 'a: 'this> BumpInto<'a> {
    /// Creates a new `BumpInto`, wrapping a slice of MaybeUninit<u8>.
    pub fn new(array: &'a mut [MaybeUninit<u8>]) -> Self {
        BumpInto {
            array: UnsafeCell::new(array),
        }
    }

    /// Tries to allocate `size` bytes with alignment `align`.
    /// Returns a null pointer on failure.
    pub fn alloc_space<S: Into<usize>, A: Into<usize>>(
        &'this self,
        size: S,
        align: A,
    ) -> *mut MaybeUninit<u8> {
        let size = size.into();
        let align = align.into();

        if size == 0 {
            // optimization for zero-sized types, as pointers to
            // such types don't have to be distinct in Rust
            return align as *mut MaybeUninit<u8>;
        }

        if size > isize::max_value() as usize {
            // Rust doesn't really support allocations this big,
            // and there's no way you'd want to put one in an
            // inline bump heap anyway. it's easier if we put
            // this check here. note that other parts of the code
            // do depend on this check being in this method!
            return ptr::null_mut();
        }

        let array = unsafe { &mut *self.array.get() };

        // since we have to do math to align our output properly,
        // we use `usize` instead of pointers
        let array_start = *array as *mut [MaybeUninit<u8>] as *mut MaybeUninit<u8> as usize;
        let current_end = array_start + array.len();

        // the highest address with enough space to store `size` bytes
        let preferred_ptr = match current_end.checked_sub(size) {
            Some(preferred_ptr) => preferred_ptr,
            None => return ptr::null_mut(),
        };

        // round down to the nearest multiple of `align`
        let aligned_ptr = (preferred_ptr / align) * align;

        if aligned_ptr < array_start {
            // not enough space -- do nothing and return null
            ptr::null_mut()
        } else {
            // bump the bump pointer and return the allocation
            *array = &mut array[..aligned_ptr - array_start];

            aligned_ptr as *mut MaybeUninit<u8>
        }
    }

    /// Tries to allocate enough space to store a `T`.
    /// Returns a properly aligned pointer to uninitialized `T` if
    /// there was enough space; otherwise returns a null pointer.
    pub fn alloc_space_for<T>(&'this self) -> *mut T {
        self.alloc_space(SizeOf::<T>::new(), AlignOf::<T>::new()) as *mut T
    }

    /// Tries to allocate enough space to store `count` number of `T`.
    /// Returns a properly aligned pointer to uninitialized `T` if
    /// there was enough space; otherwise returns a null pointer.
    pub fn alloc_space_for_n<T>(&'this self, count: usize) -> *mut T {
        let size = mem::size_of::<T>().checked_mul(count);

        match size {
            Some(size) => self.alloc_space(size, AlignOf::<T>::new()) as *mut T,
            None => ptr::null_mut(),
        }
    }

    /// Tries to allocate enough space to store a `T` and place `x` there.
    ///
    /// On success (i.e. if there was enough space) produces a mutable
    /// reference to `x` with the lifetime of this `BumpInto`.
    ///
    /// On failure, produces `x`.
    pub fn alloc<T>(&'this self, x: T) -> Result<&'this mut T, T> {
        let pointer = self.alloc_space_for::<T>();

        if pointer.is_null() {
            return Err(x);
        }

        unsafe {
            ptr::write(pointer, x);

            Ok(&mut *pointer)
        }
    }

    /// Tries to allocate enough space to store a `T` and place the result
    /// of calling `f` there.
    ///
    /// On success (i.e. if there was enough space) produces a mutable
    /// reference to the stored result with the lifetime of this `BumpInto`.
    pub fn alloc_with<T, F: FnOnce() -> T>(&'this self, f: F) -> Option<&'this mut T> {
        #[inline(always)]
        unsafe fn eval_and_write<T, F: FnOnce() -> T>(pointer: *mut T, f: F) {
            // this is an optimization borrowed from bumpalo by fitzgen
            // it's meant to help the compiler realize it can avoid a copy by
            // evaluating `f` directly into the allocated space
            ptr::write(pointer, f());
        }

        let pointer = self.alloc_space_for::<T>();

        if pointer.is_null() {
            return None;
        }

        unsafe {
            eval_and_write(pointer, f);

            Some(&mut *pointer)
        }
    }

    /// Tries to allocate enough space to store a copy of `xs` and copy
    /// `xs` into it.
    ///
    /// On success (i.e. if there was enough space) produces a mutable
    /// reference to the copy with the lifetime of this `BumpInto`.
    pub fn alloc_n<T: Copy>(&'this self, xs: &[T]) -> Option<&'this mut [T]> {
        let pointer = self.alloc_space(mem::size_of_val(xs), AlignOf::<T>::new()) as *mut T;

        if pointer.is_null() {
            return None;
        }

        unsafe {
            for (index, &x) in xs.iter().enumerate() {
                ptr::write(pointer.add(index), x);
            }

            Some(core::slice::from_raw_parts_mut(pointer, xs.len()))
        }
    }

    /// Tries to allocate enough space to store `count` number of `T` and
    /// fill it with the values produced by `iter.into_iter()`.
    ///
    /// On success (i.e. if there was enough space) produces a mutable
    /// reference to the stored results as a slice, with the lifetime of
    /// this `BumpInto`.
    ///
    /// If the iterator ends before producing enough items to fill the
    /// allocated space, the same amount of space is allocated, but the
    /// returned slice is just long enough to hold the items that were
    /// actually produced.
    pub fn alloc_n_with<T, I: IntoIterator<Item = T>>(
        &'this self,
        count: usize,
        iter: I,
    ) -> Option<&'this mut [T]> {
        #[inline(always)]
        unsafe fn iter_and_write<T, I: Iterator<Item = T>>(pointer: *mut T, mut iter: I) -> bool {
            match iter.next() {
                Some(item) => {
                    ptr::write(pointer, item);

                    false
                }
                None => true,
            }
        }

        let pointer = self.alloc_space_for_n::<T>(count);

        if pointer.is_null() {
            return None;
        }

        let mut iter = iter.into_iter();

        unsafe {
            for index in 0..count {
                if iter_and_write(pointer.add(index), &mut iter) {
                    // iterator ended before we could fill the whole space.
                    return Some(core::slice::from_raw_parts_mut(pointer, index));
                }
            }

            Some(core::slice::from_raw_parts_mut(pointer, count))
        }
    }
}

impl<'a> fmt::Debug for BumpInto<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BumpInto {{ {} bytes free }}", unsafe {
            (*self.array.get()).len()
        })
    }
}

/// Creates an uninitialized array of `MaybeUninit<u8>` on the stack,
/// suitable for taking a slice of to pass into `BumpInto::new`.
#[macro_export]
macro_rules! space {
    ($capacity:expr) => {{
        extern crate core;

        unsafe {
            core::mem::MaybeUninit::<[core::mem::MaybeUninit<u8>; $capacity]>::uninit()
                .assume_init()
        }
    }};
}

/// Creates a zeroed array of `MaybeUninit<u8>` on the stack,
/// suitable for taking a slice of to pass into `BumpInto::new`.
#[macro_export]
macro_rules! space_zeroed {
    ($capacity:expr) => {{
        extern crate core;

        unsafe {
            core::mem::MaybeUninit::<[core::mem::MaybeUninit<u8>; $capacity]>::zeroed()
                .assume_init()
        }
    }};
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn alloc() {
        let mut space = space!(32);
        let bump_into = BumpInto::new(&mut space[..]);

        let something1 = bump_into.alloc(123u64).expect("allocation 1 failed");

        assert_eq!(*something1, 123u64);

        let something2 = bump_into.alloc(7775u16).expect("allocation 2 failed");

        assert_eq!(*something1, 123u64);
        assert_eq!(*something2, 7775u16);

        let something3 = bump_into
            .alloc_with(|| 251222u64)
            .expect("allocation 3 failed");

        assert_eq!(*something1, 123u64);
        assert_eq!(*something2, 7775u16);
        assert_eq!(*something3, 251222u64);

        if bump_into.alloc_with(|| [0; 128]).is_some() {
            panic!("allocation 4 succeeded");
        }

        let something5 = bump_into.alloc(123523u32).expect("allocation 5 failed");

        assert_eq!(*something1, 123u64);
        assert_eq!(*something2, 7775u16);
        assert_eq!(*something3, 251222u64);
        assert_eq!(*something5, 123523u32);
    }

    #[test]
    fn alloc_n() {
        let mut space = space!(192);
        let bump_into = BumpInto::new(&mut space[..]);

        let something1 = bump_into
            .alloc_n(&[1u32, 258909, 1000][..])
            .expect("allocation 1 failed");

        assert_eq!(something1, &[1u32, 258909, 1000][..]);

        let something2 = bump_into
            .alloc_n(&[1u64, 258909, 1000, 0][..])
            .expect("allocation 2 failed");

        assert_eq!(something1, &[1u32, 258909, 1000][..]);
        assert_eq!(something2, &[1u64, 258909, 1000, 0][..]);

        let something3 = bump_into
            .alloc_n_with(5, core::iter::repeat(61921u16))
            .expect("allocation 3 failed");

        assert_eq!(something1, &[1u32, 258909, 1000][..]);
        assert_eq!(something2, &[1u64, 258909, 1000, 0][..]);
        assert_eq!(something3, &[61921u16; 5][..]);

        let something4 = bump_into
            .alloc_n_with(6, core::iter::once(71u64))
            .expect("allocation 4 failed");

        assert_eq!(something1, &[1u32, 258909, 1000][..]);
        assert_eq!(something2, &[1u64, 258909, 1000, 0][..]);
        assert_eq!(something3, &[61921u16; 5][..]);
        assert_eq!(something4, &[71u64][..]);

        if bump_into.alloc_n_with::<u64, _>(100, None).is_some() {
            panic!("allocation 5 succeeded")
        }

        assert_eq!(something1, &[1u32, 258909, 1000][..]);
        assert_eq!(something2, &[1u64, 258909, 1000, 0][..]);
        assert_eq!(something3, &[61921u16; 5][..]);
        assert_eq!(something4, &[71u64][..]);

        let something6 = bump_into
            .alloc_n_with::<u64, _>(6, None)
            .expect("allocation 6 failed");

        assert_eq!(something1, &[1u32, 258909, 1000][..]);
        assert_eq!(something2, &[1u64, 258909, 1000, 0][..]);
        assert_eq!(something3, &[61921u16; 5][..]);
        assert_eq!(something4, &[71u64][..]);
        assert_eq!(something6, &[]);
    }

    #[test]
    fn readme_example() {
        // allocate 64 bytes of uninitialized space on the stack
        let mut bump_into_space = space!(64);
        let bump_into = BumpInto::new(&mut bump_into_space[..]);

        // allocating an object produces a mutable reference with
        // the same lifetime as the `BumpInto` instance, or `None`
        // if there isn't enough space
        let number: &mut u64 = bump_into.alloc_with(|| 123).expect("not enough space");
        assert_eq!(*number, 123);
        *number = 50000;
        assert_eq!(*number, 50000);

        // slices can be allocated as well
        let slice: &mut [u16] = bump_into
            .alloc_n_with(5, core::iter::repeat(10))
            .expect("not enough space");
        assert_eq!(slice, &[10; 5]);
        slice[2] = 100;
        assert_eq!(slice, &[10, 10, 100, 10, 10]);
    }
}
