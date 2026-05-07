# Задача: Fused self-call оптимизация в Lake compiler

## Контекст

Lake — compiled actor-oriented language. Каждая "машина" (machine) компилируется в Cranelift IR как CPS state machine с Switch dispatch. Scheduler вызывает machine function, она исполняет один блок и возвращает next_block_id через quantum_continue_block. Fuel (quantum) декрементируется на каждом блоке.

## Проблема

Вызов `self(steps-1, acc2, acc1+acc2)` в текущем codegen генерирует ~10 dispatch блоков:

1. `pure_expr` вычисляет `steps-1` → пишет в `TEMP_VAL` → jump quantum_continue
2. **staging block**: `call rt_load_u64(ctx, TEMP_VAL)` + `call rt_load_u64(ctx, JUMP_ARGS)` + `call rt_store(args, val, 8, offset)` → jump quantum_continue
3. Повторить пункты 1-2 для `acc2` и `acc1+acc2` (ещё 4 блока)
4. `change_state_expr`: для каждого аргумента `call rt_load_u64` из JUMP_ARGS + store в VARIABLES, set BRANCH_ID → jump quantum_continue

Итого: ~10 dispatch cycles, ~12 function calls (`rt_load_u64`/`rt_store`) с bounds checking на каждый.

Benchmark: fib(100000) x8 workers = 27ms. C sequential = 862µs. Ratio = 31x.

## Цель

Если `self(...)` и **все** его аргументы `is_pure` (см. `pure_expr::is_pure`), генерировать **один блок** который:

1. Загружает `exec_start` и `vars_start` один раз (inline load, `MemFlags::trusted()`)
2. Вычисляет все аргументы через `pure_expr::fold` (inline арифметика, inline variable loads — без function calls)
3. Записывает результаты **напрямую в VARIABLES** через `builder.ins().store(MemFlags::trusted(), val, vars_start, slot * 8)` — минуя TEMP_VAL и JUMP_ARGS полностью
4. Устанавливает `BRANCH_ID` через inline store
5. Делает один `jump quantum_continue_block` с `block_id = 0`

Ожидаемый результат: ~19 инструкций вместо ~230, benchmark ~3-4ms вместо 27ms.

## Файлы для изменения

### `src/compiler/pipeline/expr/jump_expr.rs`

В функции `compile()`, в ветке `self` (строка ~151), **перед** текущей логикой staging аргументов, добавить проверку:

```rust
// Перед текущим циклом for (i, arg) in args.iter().enumerate()
// Проверяем: все аргументы pure?
if *callee_name == "self" {
    let all_pure = args.iter().all(|a| pure_expr::is_pure(a));
    if all_pure {
        if let Some(machine_name) = ctx.get_current_machine() {
            let call_hash = hash_call_args(args, state.lake_types());
            return compile_fused_self_call(
                ctx, builder, machine_ctx_var, block_id, branch_switch,
                state, &machine_name, call_hash, args,
            );
        }
    }
}
```

Новая функция `compile_fused_self_call` в том же файле (или в отдельном `fused_self_expr.rs`):

```rust
fn compile_fused_self_call(
    ctx: &mut CompilerCtx,
    builder: &mut FunctionBuilder,
    machine_ctx_var: Variable,
    block_id: i64,
    branch_switch: &mut Switch,
    state: &BranchState,
    machine_name: &str,
    call_hash: u64,
    args: &[Expr<'_>],
) -> Result<StmtOutcome> {
    let ptr_ty = ctx.module().target_config().pointer_type();

    let (target_branch_id, _var_count, arg_count) = ctx
        .lookup_branch_by_hash(machine_name, call_hash)
        .ok_or_else(|| anyhow!("No branch matching call hash {:#018x} in '{}'", call_hash, machine_name))?;

    let b = builder.create_block();
    builder.switch_to_block(b);

    // 1. Загрузить exec_start и vars_start один раз
    let ctx_ptr = builder.use_var(machine_ctx_var);
    let exec_start = builder.ins().load(ptr_ty, MemFlags::trusted(), ctx_ptr, 0);
    let vars_fp = builder.ins().load(ptr_ty, MemFlags::trusted(), exec_start, ExecCtxLayout::VARIABLES);
    let vars_start = builder.ins().load(ptr_ty, MemFlags::trusted(), vars_fp, 0);

    // 2. Вычислить все аргументы через fold (inline, без function calls)
    //    ВАЖНО: сначала вычислить ВСЕ значения, потом записать.
    //    Иначе self(acc2, acc1+acc2) перезапишет acc2 до того как вычислит acc1+acc2.
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        let val = pure_expr::fold(arg, builder, ptr_ty, Some(vars_start), state);
        values.push(val);
    }

    // 3. Записать все значения в VARIABLES напрямую
    for (i, val) in values.iter().enumerate() {
        builder.ins().store(MemFlags::trusted(), *val, vars_start, i as i32 * 8);
    }

    // 4. Установить BRANCH_ID
    let branch_id_val = builder.ins().iconst(ptr_ty, target_branch_id as i64);
    builder.ins().store(MemFlags::trusted(), branch_id_val, exec_start, ExecCtxLayout::BRANCH_ID);

    // 5. Jump в quantum_continue с block_id = 0
    let next_id = builder.ins().iconst(ptr_ty, 0);
    let qb = ctx.quantum_block();
    builder.ins().jump(qb, &[BlockArg::Value(next_id)]);

    branch_switch.set_entry(block_id as u128, b);
    Ok(StmtOutcome::StateChange { next_available: block_id + 1 })
}
```

### `src/compiler/pipeline/expr/pure_expr.rs`

Функция `fold` сейчас **приватная** (`fn fold`). Нужно сделать её **публичной**:

```rust
// Было:
fn fold(...)
// Стало:
pub fn fold(...)
```

Также `has_var` нужно сделать `pub` — она используется в `compile_fused_self_call` неявно через `fold`
(fold уже обрабатывает Var, но vars_start передаётся как Option — если ни один аргумент не содержит Var,
можно передать None, но безопаснее всегда передавать Some).

### `src/compiler/pipeline/expr/when_expr.rs`

Аналогичная оптимизация для `when` с pure condition + pure self-call bodies.
Это можно отложить на второй этап — основной выигрыш от fused self-call.

## ВАЖНО: порядок записи

Вычисление значений и запись ДОЛЖНЫ быть разделены. Если `self(acc2, acc1+acc2)`:
- Сначала: `val0 = load vars[1]` (acc2), `val1 = load vars[0] + load vars[1]` (acc1+acc2)
- Потом: `store vars[0] = val0`, `store vars[1] = val1`

Если записать vars[0] до вычисления val1 — val1 прочитает уже перезаписанное значение.
`fold` возвращает Cranelift `Value` (SSA), поэтому если все значения вычислены до первого store —
Cranelift корректно сохранит исходные значения через регистры.

## ВАЖНО: не ломать non-pure path

Текущий codegen через staging blocks + TEMP_VAL + JUMP_ARGS должен остаться как fallback для
аргументов которые не pure (содержат side effects, вызовы других машин, и т.д.).
Оптимизация только для `all args are pure` case.

## Тестирование

```sh
# Компиляция и запуск fib benchmark
RUST_LOG=info cargo run --release -- benchmark/cpu_bench.lake
./benchmark/build/cpu_bench

# Должно работать корректно (тот же вывод — 8 точек):
# .
# .
# ... (8 раз)

# Проверить также:
RUST_LOG=info cargo run --release -- examples/counter.lake
./examples/build/counter
# Ожидаемый вывод: "done\n" три раза (counter(5), counter(3), counter(7))

RUST_LOG=info cargo run --release -- examples/sum.lake
./examples/build/sum
# Ожидаемый вывод: "done\n"

# Benchmark timing:
hyperfine './benchmark/build/cpu_bench'
# Ожидание: ~3-5ms вместо текущих ~27ms
```

## Архитектура для справки

```
ExecCtx layout (40 bytes per process):
  +0  BRANCH_ID  : i64
  +8  BLOCK_ID   : i64
  +16 TEMP_VAL   : i64
  +24 VARIABLES  : i64 (fat ptr → start_addr of vars array)
  +32 JUMP_ARGS  : i64 (fat ptr → start_addr of args staging buffer)

Fat ptr layout (16 bytes):
  +0  start : i64
  +8  end   : i64

Machine function signature: fn(ctx_fat_ptr: ptr) -> stop_code: ptr
  STOP_DONE  = -1 (process finished)
  STOP_LIMIT = -2 (quantum exhausted)

Quantum loop: machine_switch → block → quantum_continue → quantum_loop → machine_switch
  quantum_continue: check if next_id == -1 → STOP_DONE, else → quantum_loop
  quantum_loop: store BLOCK_ID, decrement fuel, if fuel==0 → STOP_LIMIT, else → machine_switch
```
