use cranelift::prelude::Variable;

pub mod process_map;

pub struct Process {
    start_block: Variable,
    current_block: Variable,
}
