// Scheduler infrastructure (Cranelift IR level).
//
// The scheduler is the entity that:
//   1. Maintains a queue of active processes (each process = machine fn + ExecCtx).
//   2. Calls each machine function with its context for one block of work.
//   3. Stores the returned next-block-id back into the context.
//   4. Removes processes that return -1 (finished).
//   5. Loops until the queue is empty, then calls rt_exit(0).
//
// Current state: the scheduler loop for a single process lives in
// `RuntimeBuilder::build()` inside `rt/mod.rs`.  This module will grow to
// support multi-process scheduling once the process queue data structure and
// the coop / spawn primitives are added to the language.
//
// ## Design notes (Cranelift IR)
//
// Process queue (to be implemented):
//   [capacity: i64 | len: i64 | entries: [ProcessEntry; N]]
//   ProcessEntry {
//       machine_fn_ptr : i64,  // pointer to the compiled machine function
//       ctx_fat_ptr    : i64,  // pointer to the ExecCtx fat pointer
//   }
//
// Scheduler loop pseudocode (Cranelift IR):
//   loop:
//     for i in 0..queue.len:
//       entry = queue[i]
//       result = call entry.machine_fn_ptr(entry.ctx_fat_ptr)
//       if result == -1: queue.remove(i)
//       else: rt_store(entry.ctx_fat_ptr, result, 8, BLOCK_ID_OFFSET)
//     if queue.len == 0: rt_exit(0)
//
// Future: pub fn build_scheduler(ctx: CompilerCtx, process_defs: &[ProcessDef]) -> Result<CompilerCtx>
