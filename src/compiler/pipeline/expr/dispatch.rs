use anyhow::Result;
use cranelift::{
    codegen::ir::{BlockArg, Value},
    frontend::Switch,
    module::{DataDescription, Linkage, Module},
    prelude::{FunctionBuilder, InstBuilder, IntCC, MemFlags, Type, types},
};

use crate::compiler::{
    ctx::{
        CompilerCtx,
        registry::{BranchInfo, GuardValue},
    },
    mphf::{MphfBuilder, emit_fxhash, emit_hash_function, emit_mphf_lookup, fxhash},
    pipeline::expr::string_expr,
};

/// Find the first parameter position (0-based) that carries a literal guard
/// in any of the candidate branches.
pub fn find_first_guard_pos(candidates: &[BranchInfo]) -> usize {
    candidates
        .iter()
        .find_map(|c| c.guards.iter().position(|g| g.is_some()))
        .unwrap_or(0)
}

/// Find the most-discriminating guard position across `candidates` — i.e.
/// the position with the greatest number of distinct literal values.  Ties
/// are broken in favor of the leftmost position.  This avoids MPHF
/// duplicate-key panics when all branches share a literal at the first
/// guard position (e.g. all routes start with `"GET"`).
pub fn find_best_guard_pos(candidates: &[BranchInfo]) -> usize {
    if candidates.is_empty() {
        return 0;
    }
    let max_arity = candidates.iter().map(|c| c.guards.len()).max().unwrap_or(0);
    let mut best_pos = find_first_guard_pos(candidates);
    let mut best_distinct = 0usize;
    for pos in 0..max_arity {
        let mut seen: std::collections::HashSet<&GuardValue> = std::collections::HashSet::new();
        for c in candidates {
            if let Some(Some(g)) = c.guards.get(pos) {
                seen.insert(g);
            }
        }
        if seen.len() > best_distinct {
            best_distinct = seen.len();
            best_pos = pos;
        }
    }
    best_pos
}

/// Returns the kind of guard present in the candidates at the first guard
/// position, or `None` if no candidate has any guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
    Int,
    Str,
}

pub fn guard_kind(candidates: &[BranchInfo]) -> Option<GuardKind> {
    let pos = find_best_guard_pos(candidates);
    for branch in candidates {
        match branch.guards.get(pos).and_then(|g| g.as_ref()) {
            Some(GuardValue::Int(_)) => return Some(GuardKind::Int),
            Some(GuardValue::Str(_)) => return Some(GuardKind::Str),
            _ => continue,
        }
    }
    None
}

/// High-level dispatcher: picks between int- and string-guard variants based
/// on the kind of guards present in the candidate set.
///
/// The caller owns:
///   - loading the discriminant value from `JUMP_ARGS[disc_pos]` (always i64)
///   - choosing a stable `namespace` for the str path's data sections
///
/// For int candidates the `namespace` argument is unused.
pub fn emit_guard_select(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    ptr_ty: Type,
    candidates: &[BranchInfo],
    discriminant: Value,
    namespace: i64,
) -> Result<Value> {
    match guard_kind(candidates) {
        Some(GuardKind::Int) | None => Ok(emit_int_guard_select(
            builder,
            ptr_ty,
            candidates,
            discriminant,
        )),
        Some(GuardKind::Str) => {
            emit_str_guard_select(ctx, builder, ptr_ty, candidates, discriminant, namespace)
        }
    }
}

/// Emit a Cranelift Switch that selects a `branch_id` at runtime based on
/// an integer `discriminant` value.
///
/// Creates one trampoline block per candidate; each block stores its
/// `branch_id` as an `iconst` and jumps to a shared merge block whose
/// single block parameter carries the selected `branch_id` value.
///
/// Returns the `branch_id` `Value` (block param of the merge block).
/// After this call the builder is positioned at (and has sealed) the merge block.
///
/// Only `GuardValue::Int` guards are dispatched; `GuardValue::Str` candidates
/// are treated as the wildcard (default) case — use `emit_str_guard_select`
/// for string-guarded dispatch.
pub fn emit_int_guard_select(
    builder: &mut FunctionBuilder,
    ptr_ty: Type,
    candidates: &[BranchInfo],
    discriminant: Value,
) -> Value {
    let b_merge = builder.create_block();
    builder.append_block_param(b_merge, ptr_ty);

    let mut guard_switch = Switch::new();
    let mut wildcard_block = None::<(cranelift::prelude::Block, u128)>;

    let arm_blocks: Vec<_> = candidates.iter().map(|_| builder.create_block()).collect();

    for (i, branch) in candidates.iter().enumerate() {
        let guard = branch.guards.iter().find_map(|g| g.as_ref());
        match guard {
            Some(GuardValue::Int(v)) => {
                guard_switch.set_entry(*v as u128, arm_blocks[i]);
            }
            _ => {
                wildcard_block = Some((arm_blocks[i], branch.branch_id));
            }
        }
    }

    let default_block = wildcard_block.map(|(b, _)| b).unwrap_or(arm_blocks[0]);

    guard_switch.emit(builder, discriminant, default_block);

    for (i, branch) in candidates.iter().enumerate() {
        builder.switch_to_block(arm_blocks[i]);
        builder.seal_block(arm_blocks[i]);
        let bid = builder.ins().iconst(ptr_ty, branch.branch_id as i64);
        builder.ins().jump(b_merge, &[BlockArg::Value(bid)]);
    }

    builder.switch_to_block(b_merge);
    builder.seal_block(b_merge);
    builder.block_params(b_merge)[0]
}

/// Emit MPHF-based dispatch on a string discriminant.
///
/// `disc_fat_ptr_addr` is the i64 value loaded from `JUMP_ARGS[disc_pos]` — for
/// a string argument that is the **address of a 16-byte fat-pointer struct**
/// `[start_ptr, end_ptr]` pointing at the literal's bytes in `.rodata`.
///
/// Pipeline:
///   1. Load `(start, end)` from the fat-pointer; compute `len = end - start`.
///   2. Compute `fxhash(start, len)` of the runtime arg bytes.
///   3. MPHF lookup → `index ∈ 0..N`.
///   4. Verify: `keys[index] == fxhash_value` (rejects strings whose hash
///      does not match any literal).
///   5. Verify: `len == lit_lens[index]` (cheap reject before memcmp).
///   6. Inline byte-loop memcmp arg vs `lits_blob[lit_offsets[index]..]`
///      (rejects hash collisions where length matches but bytes differ).
///   7. On full match → Cranelift Switch on `index` selects per-arm trampoline
///      that jumps to a shared merge block carrying the chosen `branch_id`.
///   8. Any failure path → wildcard branch_id (or arm 0 if no wildcard
///      candidate exists, mirroring `emit_int_guard_select`).
///
/// `namespace` must be unique per call site (e.g. block_id) to avoid
/// data-section name collisions across multiple guards in the same module.
pub fn emit_str_guard_select(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    ptr_ty: Type,
    candidates: &[BranchInfo],
    disc_fat_ptr_addr: Value,
    namespace: i64,
) -> Result<Value> {
    let i64_ty = types::I64;

    // ── 1. partition candidates ──────────────────────────────────────────────
    let mut str_branches: Vec<(usize, &BranchInfo, Vec<u8>)> = Vec::new();
    let mut wildcard: Option<&BranchInfo> = None;

    let pos = find_best_guard_pos(candidates);
    for branch in candidates {
        match branch.guards.get(pos).and_then(|g| g.as_ref()) {
            Some(GuardValue::Str(s)) => {
                let bytes = string_expr::unescape(s);
                str_branches.push((str_branches.len(), branch, bytes));
            }
            None => {
                wildcard = Some(branch);
            }
            Some(GuardValue::Int(_)) => {
                anyhow::bail!(
                    "mixed int+str guards at the same parameter position \
                     are not supported (branch_id {})",
                    branch.branch_id
                );
            }
        }
    }

    anyhow::ensure!(
        !str_branches.is_empty(),
        "emit_str_guard_select called with no string-guarded candidates"
    );

    // ── 2. build MPHF over fxhash of literals ────────────────────────────────
    let keys: Vec<u64> = str_branches
        .iter()
        .map(|(_, _, bytes)| fxhash(bytes))
        .collect();
    let mphf = MphfBuilder::build(&keys);

    // Reorder by MPHF index so that data sections can be indexed directly
    // by `mphf.lookup(key)`.
    let mut by_index: Vec<Option<(u64, &[u8], u128)>> = vec![None; mphf.total_keys];
    for ((_, branch, bytes), &key) in str_branches.iter().zip(keys.iter()) {
        let idx = mphf.lookup(key) as usize;
        by_index[idx] = Some((key, bytes.as_slice(), branch.branch_id));
    }

    // ── 3. emit data sections ────────────────────────────────────────────────
    // disp_<ns>     : Vec<u32>  (mphf displacements)
    // keys_<ns>     : Vec<u64>  (fxhash of literal at lookup index)
    // lit_meta_<ns> : Vec<[u64; 2]>  (offset, len) per index
    // lit_blob_<ns> : concatenated literal bytes
    // bid_<ns>      : Vec<u64>  (branch_id at lookup index)
    let disp_id = ctx.module_mut().declare_data(
        &format!("guard_disp_{namespace}"),
        Linkage::Export,
        false,
        false,
    )?;
    let mut disp_desc = DataDescription::new();
    let mut disp_bytes = Vec::with_capacity(mphf.displacements.len() * 4);
    for d in &mphf.displacements {
        disp_bytes.extend_from_slice(&d.to_le_bytes());
    }
    disp_desc.define(disp_bytes.into_boxed_slice());
    ctx.module_mut().define_data(disp_id, &disp_desc)?;

    let keys_id = ctx.module_mut().declare_data(
        &format!("guard_keys_{namespace}"),
        Linkage::Export,
        false,
        false,
    )?;
    let mut keys_desc = DataDescription::new();
    let mut keys_bytes = Vec::with_capacity(mphf.total_keys * 8);
    for slot in &by_index {
        let key = slot.map(|(k, _, _)| k).unwrap_or(0);
        keys_bytes.extend_from_slice(&key.to_le_bytes());
    }
    keys_desc.define(keys_bytes.into_boxed_slice());
    ctx.module_mut().define_data(keys_id, &keys_desc)?;

    // Concatenate literal bytes into one blob; per-index meta = (offset, len).
    let mut blob: Vec<u8> = Vec::new();
    let mut meta_bytes: Vec<u8> = Vec::with_capacity(mphf.total_keys * 16);
    for slot in &by_index {
        let (offset, len) = match slot {
            Some((_, bytes, _)) => {
                let off = blob.len();
                blob.extend_from_slice(bytes);
                (off as u64, bytes.len() as u64)
            }
            None => (0, u64::MAX), // sentinel: never matches a real length
        };
        meta_bytes.extend_from_slice(&offset.to_le_bytes());
        meta_bytes.extend_from_slice(&len.to_le_bytes());
    }
    if blob.is_empty() {
        // Cranelift forbids empty data sections — push a single zero byte
        // so the section has a defined address even when N=0 literal bytes.
        blob.push(0);
    }
    let blob_id = ctx.module_mut().declare_data(
        &format!("guard_lit_blob_{namespace}"),
        Linkage::Export,
        false,
        false,
    )?;
    let mut blob_desc = DataDescription::new();
    blob_desc.define(blob.into_boxed_slice());
    ctx.module_mut().define_data(blob_id, &blob_desc)?;

    let meta_id = ctx.module_mut().declare_data(
        &format!("guard_lit_meta_{namespace}"),
        Linkage::Export,
        false,
        false,
    )?;
    let mut meta_desc = DataDescription::new();
    meta_desc.define(meta_bytes.into_boxed_slice());
    ctx.module_mut().define_data(meta_id, &meta_desc)?;

    let bid_id = ctx.module_mut().declare_data(
        &format!("guard_bid_{namespace}"),
        Linkage::Export,
        false,
        false,
    )?;
    let mut bid_desc = DataDescription::new();
    let mut bid_bytes = Vec::with_capacity(mphf.total_keys * 8);
    for slot in &by_index {
        let bid = slot.map(|(_, _, b)| b as u64).unwrap_or(u64::MAX);
        bid_bytes.extend_from_slice(&bid.to_le_bytes());
    }
    bid_desc.define(bid_bytes.into_boxed_slice());
    ctx.module_mut().define_data(bid_id, &bid_desc)?;

    // ── 4. emit IR ───────────────────────────────────────────────────────────
    let b_merge = builder.create_block();
    builder.append_block_param(b_merge, ptr_ty);

    let b_no_match = builder.create_block();
    let b_check_len = builder.create_block();
    let b_memcmp_header = builder.create_block();
    let b_memcmp_body = builder.create_block();
    let b_match = builder.create_block();

    // Load (start, end) of the runtime argument from its fat-ptr.
    let start = builder
        .ins()
        .load(ptr_ty, MemFlags::new(), disc_fat_ptr_addr, 0);
    let end = builder
        .ins()
        .load(ptr_ty, MemFlags::new(), disc_fat_ptr_addr, 8);
    let len = builder.ins().isub(end, start);

    // fxhash(start, len) — terminates the current block; builder lands in
    // the loop_exit block automatically.
    let fxhash_val = emit_fxhash(builder, start, len);

    // mphf_hash + lookup
    let mphf_hash = emit_hash_function(builder, fxhash_val, mphf.seed);
    let disp_gv = ctx.module_mut().declare_data_in_func(disp_id, builder.func);
    let disp_ptr = builder.ins().global_value(ptr_ty, disp_gv);
    let index_i32 = emit_mphf_lookup(builder, &mphf, mphf_hash, disp_ptr);
    let index = builder.ins().uextend(i64_ty, index_i32);

    // Verify hash: keys[index] == fxhash_val.
    let keys_gv = ctx.module_mut().declare_data_in_func(keys_id, builder.func);
    let keys_ptr = builder.ins().global_value(ptr_ty, keys_gv);
    let key_offset = builder.ins().imul_imm(index, 8);
    let key_addr = builder.ins().iadd(keys_ptr, key_offset);
    let stored_key = builder.ins().load(i64_ty, MemFlags::trusted(), key_addr, 0);
    let hash_eq = builder.ins().icmp(IntCC::Equal, stored_key, fxhash_val);
    builder
        .ins()
        .brif(hash_eq, b_check_len, &[], b_no_match, &[]);

    // ── b_check_len: lit_meta[index].len == len ──────────────────────────────
    builder.switch_to_block(b_check_len);
    builder.seal_block(b_check_len);
    let meta_gv = ctx.module_mut().declare_data_in_func(meta_id, builder.func);
    let meta_ptr = builder.ins().global_value(ptr_ty, meta_gv);
    let meta_offset = builder.ins().imul_imm(index, 16);
    let meta_addr = builder.ins().iadd(meta_ptr, meta_offset);
    let lit_offset = builder
        .ins()
        .load(i64_ty, MemFlags::trusted(), meta_addr, 0);
    let lit_len = builder
        .ins()
        .load(i64_ty, MemFlags::trusted(), meta_addr, 8);
    let len_eq = builder.ins().icmp(IntCC::Equal, lit_len, len);

    // Compute lit_start = blob_ptr + lit_offset for the memcmp loop.
    let blob_gv = ctx.module_mut().declare_data_in_func(blob_id, builder.func);
    let blob_ptr = builder.ins().global_value(ptr_ty, blob_gv);
    let lit_start = builder.ins().iadd(blob_ptr, lit_offset);

    // Init memcmp loop: i=0; passes (lit_start, len, i) into header.
    builder.append_block_param(b_memcmp_header, i64_ty); // i
    builder.append_block_param(b_memcmp_body, i64_ty); // i
    let zero = builder.ins().iconst(i64_ty, 0);
    builder.ins().brif(
        len_eq,
        b_memcmp_header,
        &[BlockArg::Value(zero)],
        b_no_match,
        &[],
    );

    // ── b_memcmp_header: while (i < len) compare next byte, else match ───────
    builder.switch_to_block(b_memcmp_header);
    let i = builder.block_params(b_memcmp_header)[0];
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, i, len);
    builder.ins().brif(
        in_bounds,
        b_memcmp_body,
        &[BlockArg::Value(i)],
        b_match,
        &[],
    );

    // ── b_memcmp_body: byte_eq → header(i+1) else b_no_match ─────────────────
    builder.switch_to_block(b_memcmp_body);
    let bi = builder.block_params(b_memcmp_body)[0];
    let arg_byte_addr = builder.ins().iadd(start, bi);
    let lit_byte_addr = builder.ins().iadd(lit_start, bi);
    let arg_byte = builder
        .ins()
        .load(types::I8, MemFlags::trusted(), arg_byte_addr, 0);
    let lit_byte = builder
        .ins()
        .load(types::I8, MemFlags::trusted(), lit_byte_addr, 0);
    let byte_eq = builder.ins().icmp(IntCC::Equal, arg_byte, lit_byte);
    let i_next = builder.ins().iadd_imm(bi, 1);
    builder.ins().brif(
        byte_eq,
        b_memcmp_header,
        &[BlockArg::Value(i_next)],
        b_no_match,
        &[],
    );
    builder.seal_block(b_memcmp_body);
    builder.seal_block(b_memcmp_header);

    // ── b_match: load branch_id from bid table[index], jump to merge ────────
    builder.switch_to_block(b_match);
    builder.seal_block(b_match);
    let bid_gv = ctx.module_mut().declare_data_in_func(bid_id, builder.func);
    let bid_ptr = builder.ins().global_value(ptr_ty, bid_gv);
    let bid_offset = builder.ins().imul_imm(index, 8);
    let bid_addr = builder.ins().iadd(bid_ptr, bid_offset);
    let bid_val = builder.ins().load(ptr_ty, MemFlags::trusted(), bid_addr, 0);
    builder.ins().jump(b_merge, &[BlockArg::Value(bid_val)]);

    // ── b_no_match: jump merge with wildcard branch_id (or first arm if none)
    builder.switch_to_block(b_no_match);
    builder.seal_block(b_no_match);
    let fallback_branch_id = wildcard
        .map(|w| w.branch_id)
        .or_else(|| candidates.first().map(|c| c.branch_id))
        .unwrap_or(0);
    let fb_val = builder.ins().iconst(ptr_ty, fallback_branch_id as i64);
    builder.ins().jump(b_merge, &[BlockArg::Value(fb_val)]);

    // ── b_merge: caller continues here with the selected branch_id ──────────
    builder.switch_to_block(b_merge);
    builder.seal_block(b_merge);
    Ok(builder.block_params(b_merge)[0])
}
