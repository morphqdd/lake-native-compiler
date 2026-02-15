use std::collections::HashMap;

use anyhow::{Result, anyhow};

/// Information about a single branch of a machine.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    /// Number of non-default parameters in the branch pattern.
    pub param_count: usize,
    /// Branch identifier used as the switch entry in the machine's jump table.
    pub branch_id: u128,
    /// Number of local variables live in this branch.
    pub var_count: usize,
}

/// Per-machine registry entry.
#[derive(Debug, Default)]
pub struct MachineInfo {
    /// Map from pattern hash → branch info.
    branches: HashMap<u64, BranchInfo>,
}

impl MachineInfo {
    pub fn insert_branch(&mut self, hash: u64, info: BranchInfo) {
        self.branches.insert(hash, info);
    }

    /// Find the branch whose `param_count` matches `count`.
    /// Returns `None` if no such branch exists.
    pub fn branch_by_param_count(&self, count: usize) -> Option<&BranchInfo> {
        self.branches
            .values()
            .find(|b| b.param_count == count)
    }

    /// Find the branch whose `branch_id` matches `id`.
    pub fn branch_by_id(&self, id: u128) -> Option<&BranchInfo> {
        self.branches.values().find(|b| b.branch_id == id)
    }
}

/// Registry for all machines and their branches compiled so far.
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
            .insert_branch(
                hash,
                BranchInfo {
                    param_count,
                    branch_id,
                    var_count,
                },
            );
        Ok(())
    }

    /// Look up the branch_id for a machine whose branch has the given param_count.
    pub fn branch_id_by_param_count(&self, machine: &str, param_count: usize) -> Option<u128> {
        self.machines
            .get(machine)?
            .branch_by_param_count(param_count)
            .map(|b| b.branch_id)
    }

    /// Look up (branch_id, var_count, param_count) by the pattern hash.
    /// This is the primary O(1) dispatch lookup: hash(arg_types) → branch.
    pub fn branch_by_hash(&self, machine: &str, hash: u64) -> Option<(u128, usize, usize)> {
        let info = self.machines.get(machine)?.branches.get(&hash)?;
        Some((info.branch_id, info.var_count, info.param_count))
    }

    /// Look up the var_count for the branch with the given branch_id, scoped to a specific machine.
    pub fn var_count_by_branch_id(&self, machine: &str, branch_id: u128) -> Option<usize> {
        self.machines
            .get(machine)?
            .branch_by_id(branch_id)
            .map(|b| b.var_count)
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
        // Wrong machine name returns None even for valid branch_id.
        assert_eq!(reg.var_count_by_branch_id("ghost", 42), None);
    }

    #[test]
    fn insert_branch_into_unknown_machine_fails() {
        let mut reg = MachineRegistry::default();
        assert!(reg.insert_branch("ghost", 0, 0, 0, 0).is_err());
    }
}
