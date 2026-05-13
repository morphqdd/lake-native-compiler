use cranelift::{
    module::Module,
    prelude::{FunctionBuilder, InstBuilder, MemFlags, Type, Value},
};

use crate::compiler::ctx::CompilerCtx;

/// Runtime execution context layout.
/// Single source of truth for all field offsets.
///
/// Memory layout (80 bytes):
/// +0  branch_id    : i64  — which branch of the machine is active
/// +8  block_id     : i64  — which block inside the branch to execute next
/// +16 temp_val     : i64  — scratch register for passing values between blocks
/// +24 variables    : i64  — fat ptr (start addr) to the variables array
/// +32 jump_args    : i64  — fat ptr (start addr) to the jump-arguments array
/// +40 mailbox_fat  : i64  — fat ptr to ring-buffer mailbox (256 × 8 bytes)
/// +48 mailbox_head : i64  — read index (consumer)
/// +56 mailbox_tail : i64  — write index (producer)
/// +64 own_pid      : i64  — process_ctx fat-ptr address of THIS actor.
/// +72 is_dying     : i64  — non-zero = actor crashed inside a rt-* call and
///                            must be removed at the next quantum boundary.
///                            machine.rs::quantum_loop_block reads this and
///                            returns STOP_DONE so the scheduler unlinks the
///                            actor without unwinding Cranelift's call stack.
pub struct ExecCtxLayout;

impl ExecCtxLayout {
    pub const SIZE: i32 = 80;
    pub const BRANCH_ID: i32 = 0;
    pub const BLOCK_ID: i32 = 8;
    pub const TEMP_VAL: i32 = 16;
    pub const VARIABLES: i32 = 24;
    pub const JUMP_ARGS: i32 = 32;
    pub const MAILBOX_FAT: i32 = 40;
    pub const MAILBOX_HEAD: i32 = 48;
    pub const MAILBOX_TAIL: i32 = 56;
    pub const OWN_PID: i32 = 64;
    pub const IS_DYING: i32 = 72;

    /// Emit a direct load of a field from a raw ctx pointer.
    /// `ctx_ptr` must point to the start of the ExecCtx data (not the fat ptr).
    pub fn load(builder: &mut FunctionBuilder, ty: Type, ctx_ptr: Value, offset: i32) -> Value {
        builder.ins().load(ty, MemFlags::new(), ctx_ptr, offset)
    }

    /// Emit a direct store of a value into a field via a raw ctx pointer.
    pub fn store(builder: &mut FunctionBuilder, val: Value, ctx_ptr: Value, offset: i32) {
        builder.ins().store(MemFlags::new(), val, ctx_ptr, offset);
    }

    pub fn set_next_block(
        exec_ctx: Value,
        next_block: Value,
        ctx: &mut CompilerCtx,
        builder: &mut FunctionBuilder,
    ) {
        let ptr_ty = ctx.module().target_config().pointer_type();
        // INLINED: was rt_store(exec_ctx, next_block, 8, BLOCK_ID)
        let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), exec_ctx, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), next_block, exec_start, Self::BLOCK_ID);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_size_is_80() {
        assert_eq!(ExecCtxLayout::SIZE, 80);
    }

    #[test]
    fn field_offsets_are_correct() {
        assert_eq!(ExecCtxLayout::BRANCH_ID, 0);
        assert_eq!(ExecCtxLayout::BLOCK_ID, 8);
        assert_eq!(ExecCtxLayout::TEMP_VAL, 16);
        assert_eq!(ExecCtxLayout::VARIABLES, 24);
        assert_eq!(ExecCtxLayout::JUMP_ARGS, 32);
        assert_eq!(ExecCtxLayout::MAILBOX_FAT, 40);
        assert_eq!(ExecCtxLayout::MAILBOX_HEAD, 48);
        assert_eq!(ExecCtxLayout::MAILBOX_TAIL, 56);
        assert_eq!(ExecCtxLayout::OWN_PID, 64);
        assert_eq!(ExecCtxLayout::IS_DYING, 72);
    }

    #[test]
    fn last_field_fits_within_size() {
        assert!(ExecCtxLayout::IS_DYING + 8 <= ExecCtxLayout::SIZE);
    }
}
