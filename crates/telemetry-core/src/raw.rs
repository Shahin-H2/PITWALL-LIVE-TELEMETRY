//! Bounds-checked little-endian primitive reads.
//!
//! Every simulator sends little-endian, tightly packed structs. The temptation
//! is to `transmute` the buffer into a `#[repr(C, packed)]` struct — which is
//! what the original `engine.c` did — but that approach has two failure modes
//! that bite in production:
//!
//! 1. A short or malformed packet reads past the end of the buffer. In C that
//!    is silent memory corruption; a hostile or merely buggy sender on the
//!    network can walk your stack.
//! 2. Unaligned field access on `#[repr(packed)]` structs is UB in Rust and
//!    a real fault on some targets.
//!
//! These accessors compile down to the same single unaligned load the struct
//! cast would produce (LLVM elides the bounds check when the offset is a
//! constant and the length was already validated), but they return `None`
//! instead of reading garbage.

#[inline(always)]
pub fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

#[inline(always)]
pub fn i8_at(b: &[u8], off: usize) -> Option<i8> {
    b.get(off).map(|v| *v as i8)
}

#[inline(always)]
pub fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

#[inline(always)]
pub fn i16_at(b: &[u8], off: usize) -> Option<i16> {
    Some(i16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

#[inline(always)]
pub fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

#[inline(always)]
pub fn i32_at(b: &[u8], off: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

#[inline(always)]
pub fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

#[inline(always)]
pub fn f32_at(b: &[u8], off: usize) -> Option<f32> {
    Some(f32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Read four consecutive f32s — the per-wheel array shape every sim uses.
#[inline(always)]
pub fn f32x4_at(b: &[u8], off: usize) -> Option<[f32; 4]> {
    Some([
        f32_at(b, off)?,
        f32_at(b, off + 4)?,
        f32_at(b, off + 8)?,
        f32_at(b, off + 12)?,
    ])
}

/// Reorder a wheel array from the F1 games' `[RL, RR, FL, FR]` into our
/// canonical `[FL, FR, RL, RR]`.
///
/// This is the single easiest place in the whole codebase to introduce a bug
/// that looks like a physics problem. The F1 titles are the odd one out; Forza
/// and Assetto Corsa both send front-axle-first.
#[inline(always)]
pub fn from_f1_wheel_order<T: Copy>(v: [T; 4]) -> [T; 4] {
    [v[2], v[3], v[0], v[1]]
}
