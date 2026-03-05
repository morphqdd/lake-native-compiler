use cranelift::prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, TrapCode, Type, Value};

/// Fat pointer layout.
///
/// A fat pointer is a 16-byte header stored in memory:
///   +0  start: i64  — address of the first byte of the data region
///   +8  end:   i64  — address one past the last byte of the data region
///
/// The pointer to this header is what gets passed around.
/// Bounds are always checked: `start + offset + size <= end`.
pub struct FatPtrLayout;

impl FatPtrLayout {
    pub const SIZE: usize = 16;
    pub const START_OFFSET: i32 = 0;
    pub const END_OFFSET: i32 = 8;

    /// Load the `start` field from a fat pointer.
    pub fn load_start(builder: &mut FunctionBuilder, ty: Type, fat_ptr: Value) -> Value {
        builder
            .ins()
            .load(ty, MemFlags::new(), fat_ptr, Self::START_OFFSET)
    }

    /// Load the `end` field from a fat pointer.
    pub fn load_end(builder: &mut FunctionBuilder, ty: Type, fat_ptr: Value) -> Value {
        builder
            .ins()
            .load(ty, MemFlags::new(), fat_ptr, Self::END_OFFSET)
    }

    /// Store the `start` field into a fat pointer.
    pub fn store_start(builder: &mut FunctionBuilder, fat_ptr: Value, start: Value) {
        builder
            .ins()
            .store(MemFlags::new(), start, fat_ptr, Self::START_OFFSET);
    }

    /// Store the `end` field into a fat pointer.
    pub fn store_end(builder: &mut FunctionBuilder, fat_ptr: Value, end: Value) {
        builder
            .ins()
            .store(MemFlags::new(), end, fat_ptr, Self::END_OFFSET);
    }

    /// Initialise a fat pointer to cover `[data_ptr, data_ptr + byte_len)`.
    pub fn init(
        builder: &mut FunctionBuilder,
        fat_ptr: Value,
        data_ptr: Value,
        byte_len: i64,
    ) {
        let end = builder.ins().iadd_imm(data_ptr, byte_len);
        Self::store_start(builder, fat_ptr, data_ptr);
        Self::store_end(builder, fat_ptr, end);
    }

    /// Emit a bounds check for an access of `size` bytes at `start + offset`.
    /// Traps with `TrapCode::unwrap_user(32)` if the access would be out of bounds.
    pub fn bounds_check(
        builder: &mut FunctionBuilder,
        ty: Type,
        fat_ptr: Value,
        offset: Value,
        size: Value,
    ) {
        let start = Self::load_start(builder, ty, fat_ptr);
        let end = Self::load_end(builder, ty, fat_ptr);
        let access_ptr = builder.ins().iadd(start, offset);
        let access_end = builder.ins().iadd(access_ptr, size);
        let in_bounds = builder
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, access_end, end);
        builder
            .ins()
            .trapz(in_bounds, TrapCode::unwrap_user(32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_16() {
        assert_eq!(FatPtrLayout::SIZE, 16);
    }

    #[test]
    fn offsets_are_correct() {
        assert_eq!(FatPtrLayout::START_OFFSET, 0);
        assert_eq!(FatPtrLayout::END_OFFSET, 8);
    }

    #[test]
    fn end_field_fits_within_size() {
        assert!(FatPtrLayout::END_OFFSET as usize + 8 <= FatPtrLayout::SIZE);
    }
}
