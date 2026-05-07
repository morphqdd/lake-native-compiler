//! Cranelift codegen for MPHF lookup.

use super::Mphf;
use cranelift::codegen::ir::BlockArg;
use cranelift::prelude::{FunctionBuilder, InstBuilder, MemFlags, Value};

/// Generates Cranelift code for MPHF lookup.
///
/// Input: hash_value (u64)
/// Output: index (u32) in range 0..N-1
///
/// Algorithm:
/// ```ignore
/// bucket_id = hash_value % num_buckets
/// displacement = displacements[bucket_id]
/// index = (hash_value + displacement) % total_keys
/// return index
/// ```
pub fn emit_mphf_lookup(
    builder: &mut FunctionBuilder,
    mphf: &Mphf,
    hash_value: Value,
    displacements_ptr: Value,
) -> Value {
    let i64_ty = cranelift::prelude::types::I64;
    let i32_ty = cranelift::prelude::types::I32;

    // 1. bucket_id = hash_value % num_buckets
    let num_buckets = builder.ins().iconst(i64_ty, mphf.num_buckets as i64);
    let bucket_id = builder.ins().urem(hash_value, num_buckets);

    // 2. displacement = displacements[bucket_id]
    //    offset = bucket_id * 4 (u32 = 4 bytes)
    let offset = builder.ins().imul_imm(bucket_id, 4);
    let displacement_addr = builder.ins().iadd(displacements_ptr, offset);
    let displacement_32 = builder
        .ins()
        .load(i32_ty, MemFlags::trusted(), displacement_addr, 0);
    let displacement = builder.ins().uextend(i64_ty, displacement_32);

    // 3. index = (hash_value + displacement) % total_keys
    let sum = builder.ins().iadd(hash_value, displacement);
    let total_keys = builder.ins().iconst(i64_ty, mphf.total_keys as i64);
    let index_64 = builder.ins().urem(sum, total_keys);
    let index = builder.ins().ireduce(i32_ty, index_64);

    index
}

/// Generates Cranelift IR for FNV-1a hash over byte data.
///
/// Equivalent to the Rust `fxhash()` function:
/// ```ignore
/// let mut hash = 0xcbf29ce484222325u64;
/// for &byte in data {
///     hash ^= byte as u64;
///     hash = hash.wrapping_mul(0x100000001b3);
/// }
/// ```
///
/// Input: data_ptr (pointer to bytes), data_len (i64)
/// Output: hash (u64)
///
/// NOTE: This terminates the current block. After this call,
/// the builder is positioned at a new block (loop exit).
pub fn emit_fxhash(builder: &mut FunctionBuilder, data_ptr: Value, data_len: Value) -> Value {
    use cranelift::prelude::{IntCC, types};

    let i64_ty = types::I64;
    let i8_ty = types::I8;

    // Create loop blocks
    let loop_header = builder.create_block();
    let loop_body = builder.create_block();
    let loop_exit = builder.create_block();

    // loop_header(hash: i64, i: i64)
    builder.append_block_param(loop_header, i64_ty);
    builder.append_block_param(loop_header, i64_ty);

    // loop_exit(hash: i64)
    builder.append_block_param(loop_exit, i64_ty);

    // Current block → jump to loop with initial values
    let hash_init = builder.ins().iconst(i64_ty, 0xcbf29ce484222325u64 as i64);
    let i_init = builder.ins().iconst(i64_ty, 0);
    builder.ins().jump(
        loop_header,
        &[BlockArg::Value(hash_init), BlockArg::Value(i_init)],
    );

    // Loop header: while i < len
    builder.switch_to_block(loop_header);
    let hash = builder.block_params(loop_header)[0];
    let i = builder.block_params(loop_header)[1];
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, i, data_len);
    builder.ins().brif(
        in_bounds,
        loop_body,
        &[],
        loop_exit,
        &[BlockArg::Value(hash)],
    );

    // Loop body: hash ^= byte; hash *= prime; i++
    builder.switch_to_block(loop_body);
    let byte_ptr = builder.ins().iadd(data_ptr, i);
    let byte_val = builder.ins().load(i8_ty, MemFlags::trusted(), byte_ptr, 0);
    let byte_ext = builder.ins().uextend(i64_ty, byte_val);
    let hash_xor = builder.ins().bxor(hash, byte_ext);
    let prime = builder.ins().iconst(i64_ty, 0x100000001b3u64 as i64);
    let hash_mul = builder.ins().imul(hash_xor, prime);
    let i_next = builder.ins().iadd_imm(i, 1);
    builder.ins().jump(
        loop_header,
        &[BlockArg::Value(hash_mul), BlockArg::Value(i_next)],
    );

    // Seal loop blocks (all predecessors known)
    builder.seal_block(loop_body);
    builder.seal_block(loop_header);
    builder.seal_block(loop_exit);

    // Builder is now at loop_exit
    builder.switch_to_block(loop_exit);
    builder.block_params(loop_exit)[0]
}

/// Generates inline MPHF-internal hash for a key.
///
/// Input: key (u64)
/// Output: hash (u64)
pub fn emit_hash_function(builder: &mut FunctionBuilder, key: Value, seed: u64) -> Value {
    let i64_ty = cranelift::prelude::types::I64;

    // hash = key + seed
    let seed_val = builder.ins().iconst(i64_ty, seed as i64);
    let mut hash = builder.ins().iadd(key, seed_val);

    // hash ^= hash >> 33
    let shift1 = builder.ins().ushr_imm(hash, 33);
    hash = builder.ins().bxor(hash, shift1);

    // hash *= 0xff51afd7ed558ccd
    let mult1 = builder.ins().iconst(i64_ty, 0xff51afd7ed558ccd_u64 as i64);
    hash = builder.ins().imul(hash, mult1);

    // hash ^= hash >> 33
    let shift2 = builder.ins().ushr_imm(hash, 33);
    hash = builder.ins().bxor(hash, shift2);

    // hash *= 0xc4ceb9fe1a85ec53
    let mult2 = builder.ins().iconst(i64_ty, 0xc4ceb9fe1a85ec53_u64 as i64);
    hash = builder.ins().imul(hash, mult2);

    // hash ^= hash >> 33
    let shift3 = builder.ins().ushr_imm(hash, 33);
    hash = builder.ins().bxor(hash, shift3);

    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift::prelude::*;

    fn make_test_func(
        sig: Signature,
    ) -> (cranelift::codegen::ir::Function, FunctionBuilderContext) {
        let func = cranelift::codegen::ir::Function::with_name_signature(
            cranelift::codegen::ir::UserFuncName::testcase("test"),
            sig,
        );
        let ctx = FunctionBuilderContext::new();
        (func, ctx)
    }

    #[test]
    fn test_emit_hash() {
        let mut sig = Signature::new(cranelift::prelude::isa::CallConv::SystemV);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));

        let (mut func, mut ctx) = make_test_func(sig);
        let mut builder = FunctionBuilder::new(&mut func, &mut ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);

        let key = builder.block_params(entry)[0];
        let hash = emit_hash_function(&mut builder, key, 42);

        builder.ins().return_(&[hash]);
        builder.seal_all_blocks();
        builder.finalize();
    }

    #[test]
    fn test_emit_fxhash() {
        // fn(data_ptr: i64, data_len: i64) -> i64
        let mut sig = Signature::new(cranelift::prelude::isa::CallConv::SystemV);
        sig.params.push(AbiParam::new(types::I64)); // data_ptr
        sig.params.push(AbiParam::new(types::I64)); // data_len
        sig.returns.push(AbiParam::new(types::I64)); // hash

        let (mut func, mut ctx) = make_test_func(sig);
        let mut builder = FunctionBuilder::new(&mut func, &mut ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let data_ptr = builder.block_params(entry)[0];
        let data_len = builder.block_params(entry)[1];

        // emit_fxhash terminates current block, moves to loop_exit
        let hash = emit_fxhash(&mut builder, data_ptr, data_len);

        builder.ins().return_(&[hash]);
        builder.seal_all_blocks();
        builder.finalize();

        // Verify the generated IR has the expected structure
        let ir = func.to_string();
        assert!(ir.contains("brif"), "should have loop condition branch");
        assert!(ir.contains("bxor"), "should have XOR for hash mixing");
        assert!(ir.contains("imul"), "should have MUL for hash mixing");
    }
}
