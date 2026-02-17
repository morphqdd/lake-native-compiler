use std::collections::HashMap;

use anyhow::{Result, anyhow};

/// Information about a single branch of a machine.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    /// Pattern hash — computed once in the index pre-pass.
    pub hash: u64,
    /// Number of non-default parameters in the branch pattern.
    pub param_count: usize,
    /// Branch identifier used as the switch entry in the machine's jump table.
    pub branch_id: u128,
    /// Number of local variables live in this branch.
    /// Set to an estimated value during the pre-pass and updated with the
    /// exact count after the branch is compiled.
    pub var_count: usize,
}

/// Per-machine registry entry.
///
/// Maintains two indices:
/// - `by_id`   — primary index keyed by `branch_id` (used during compilation
///               to fetch the pre-computed hash and to update `var_count`).
/// - `by_hash` — dispatch index keyed by pattern hash (used at runtime for
///               O(1) branch lookup in spawn / state-change calls).
#[derive(Debug, Default)]
pub struct MachineInfo {
    by_id: HashMap<u128, BranchInfo>,
    by_hash: HashMap<u64, u128>,
}

impl MachineInfo {
    pub fn insert_branch(&mut self, info: BranchInfo) {
        self.by_hash.insert(info.hash, info.branch_id);
        self.by_id.insert(info.branch_id, info);
    }

    /// Look up branch info by pattern hash (O(1) dispatch path).
    pub fn branch_by_hash(&self, hash: u64) -> Option<&BranchInfo> {
        let id = self.by_hash.get(&hash)?;
        self.by_id.get(id)
    }

    /// Look up branch info by branch id.
    pub fn branch_by_id(&self, id: u128) -> Option<&BranchInfo> {
        self.by_id.get(&id)
    }

    /// Find the branch whose `param_count` matches `count`.
    pub fn branch_by_param_count(&self, count: usize) -> Option<&BranchInfo> {
        self.by_id.values().find(|b| b.param_count == count)
    }

    /// Update `var_count` for a branch after it has been fully compiled.
    pub fn update_var_count(&mut self, branch_id: u128, var_count: usize) {
        if let Some(info) = self.by_id.get_mut(&branch_id) {
            info.var_count = var_count;
        }
    }

    /// Maximum `var_count` across all branches of this machine.
    /// Used to size the VARIABLES buffer so any state transition is safe.
    pub fn max_var_count(&self) -> usize {
        self.by_id.values().map(|b| b.var_count).max().unwrap_or(0)
    }
}

/// Registry for all machines and their branches.
#[derive(Debug, Default)]
pub struct MachineRegistry {
    machines: HashMap<String, MachineInfo>,
}

impl MachineRegistry {
    /// Register a new machine. Must be called before `insert_branch`.
    pub fn add_machine(&mut self, name: &str) {
        self.machines
            .entry(name.to_string())
            .or_insert_with(MachineInfo::default);
    }

    /// Insert a branch into an already-registered machine.
    pub fn insert_branch(
        &mut self,
        machine: &str,
        hash: u64,
        param_count: usize,
        branch_id: u128,
        var_count: usize,
    ) -> Result<()> {
        self.machines
            .get_mut(machine)
            .ok_or_else(|| anyhow!("Machine '{machine}' is not registered"))?
            .insert_branch(BranchInfo {
                hash,
                param_count,
                branch_id,
                var_count,
            });
        Ok(())
    }

    /// Return the pre-computed pattern hash for a specific branch.
    pub fn hash_by_branch_id(&self, machine: &str, branch_id: u128) -> Option<u64> {
        self.machines
            .get(machine)?
            .branch_by_id(branch_id)
            .map(|b| b.hash)
    }

    /// Update the exact `var_count` for a branch after compilation.
    pub fn update_var_count(&mut self, machine: &str, branch_id: u128, var_count: usize) {
        if let Some(info) = self.machines.get_mut(machine) {
            info.update_var_count(branch_id, var_count);
        }
    }

    /// Look up the branch_id for a machine whose branch has the given param_count.
    pub fn branch_id_by_param_count(&self, machine: &str, param_count: usize) -> Option<u128> {
        self.machines
            .get(machine)?
            .branch_by_param_count(param_count)
            .map(|b| b.branch_id)
    }

    /// O(1) dispatch lookup: hash(arg_types) → (branch_id, var_count, param_count).
    pub fn branch_by_hash(&self, machine: &str, hash: u64) -> Option<(u128, usize, usize)> {
        let info = self.machines.get(machine)?.branch_by_hash(hash)?;
        Some((info.branch_id, info.var_count, info.param_count))
    }

    /// Look up the var_count for the branch with the given branch_id, scoped to a machine.
    pub fn var_count_by_branch_id(&self, machine: &str, branch_id: u128) -> Option<usize> {
        self.machines
            .get(machine)?
            .branch_by_id(branch_id)
            .map(|b| b.var_count)
    }

    /// Maximum `var_count` across all branches of `machine`.
    pub fn max_var_count(&self, machine: &str) -> Option<usize> {
        self.machines.get(machine).map(|m| m.max_var_count())
    }

    /// Iterate over all registered machine names.
    pub fn machine_names(&self) -> impl Iterator<Item = &str> {
        self.machines.keys().map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> MachineRegistry {
        let mut reg = MachineRegistry::default();
        reg.add_machine("main");
        reg.insert_branch("main", 0xABCD, 0, 42, 3).unwrap();
        reg.insert_branch("main", 0xEF01, 1, 43, 5).unwrap();
        reg
    }

    #[test]
    fn branch_id_by_param_count() {
        let reg = make_registry();
        assert_eq!(reg.branch_id_by_param_count("main", 0), Some(42));
        assert_eq!(reg.branch_id_by_param_count("main", 1), Some(43));
        assert_eq!(reg.branch_id_by_param_count("main", 2), None);
    }

    #[test]
    fn var_count_by_branch_id() {
        let reg = make_registry();
        assert_eq!(reg.var_count_by_branch_id("main", 42), Some(3));
        assert_eq!(reg.var_count_by_branch_id("main", 43), Some(5));
        assert_eq!(reg.var_count_by_branch_id("main", 99), None);
        assert_eq!(reg.var_count_by_branch_id("ghost", 42), None);
    }

    #[test]
    fn hash_by_branch_id() {
        let reg = make_registry();
        assert_eq!(reg.hash_by_branch_id("main", 42), Some(0xABCD));
        assert_eq!(reg.hash_by_branch_id("main", 43), Some(0xEF01));
        assert_eq!(reg.hash_by_branch_id("main", 99), None);
    }

    #[test]
    fn update_var_count() {
        let mut reg = make_registry();
        reg.update_var_count("main", 42, 99);
        assert_eq!(reg.var_count_by_branch_id("main", 42), Some(99));
        // other branch unchanged
        assert_eq!(reg.var_count_by_branch_id("main", 43), Some(5));
    }

    #[test]
    fn insert_branch_into_unknown_machine_fails() {
        let mut reg = MachineRegistry::default();
        assert!(reg.insert_branch("ghost", 0, 0, 0, 0).is_err());
    }
}
