use std::collections::HashMap;

use anyhow::{Result, anyhow};

/// A compile-time literal guard value for a single branch parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardValue {
    Int(i64),
    Str(String),
}

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
    pub var_count: usize,
    /// Per-parameter literal guards: `None` = any value (wildcard / binding),
    /// `Some(v)` = branch only matches when arg equals `v`.
    pub guards: Vec<Option<GuardValue>>,
}

impl BranchInfo {
    /// True when all guard slots are `None` (no literal constraints).
    pub fn is_wildcard_branch(&self) -> bool {
        self.guards.iter().all(|g| g.is_none())
    }
}

/// Per-machine registry entry.
///
/// Maintains two indices:
/// - `by_id`   — primary index keyed by `branch_id`.
/// - `by_hash` — dispatch index keyed by pattern hash → Vec of branch_ids.
///               Multiple branches can share the same type hash when they differ
///               only in literal guards (e.g. `0 i64` and `n i64`).
#[derive(Debug, Default)]
pub struct MachineInfo {
    by_id: HashMap<u128, BranchInfo>,
    by_hash: HashMap<u64, Vec<u128>>,
}

impl MachineInfo {
    pub fn insert_branch(&mut self, info: BranchInfo) {
        self.by_hash
            .entry(info.hash)
            .or_default()
            .push(info.branch_id);
        self.by_id.insert(info.branch_id, info);
    }

    /// Look up the first branch with this hash (fast path when no literal guards).
    pub fn branch_by_hash(&self, hash: u64) -> Option<&BranchInfo> {
        let ids = self.by_hash.get(&hash)?;
        self.by_id.get(ids.first()?)
    }

    /// Look up ALL branches that share this type hash.
    /// Returns them in insertion order (branch_id order).
    pub fn branches_for_hash(&self, hash: u64) -> Vec<&BranchInfo> {
        match self.by_hash.get(&hash) {
            Some(ids) => ids.iter().filter_map(|id| self.by_id.get(id)).collect(),
            None => vec![],
        }
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
        guards: Vec<Option<GuardValue>>,
    ) -> Result<()> {
        self.machines
            .get_mut(machine)
            .ok_or_else(|| anyhow!("Machine '{machine}' is not registered"))?
            .insert_branch(BranchInfo {
                hash,
                param_count,
                branch_id,
                var_count,
                guards,
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

    /// O(1) dispatch lookup (single-branch fast path): hash → (branch_id, var_count, param_count).
    pub fn branch_by_hash(&self, machine: &str, hash: u64) -> Option<(u128, usize, usize)> {
        let info = self.machines.get(machine)?.branch_by_hash(hash)?;
        Some((info.branch_id, info.var_count, info.param_count))
    }

    /// Return ALL branches matching the given type hash (for literal-guard dispatch).
    pub fn branches_for_type_hash(&self, machine: &str, hash: u64) -> Vec<BranchInfo> {
        self.machines
            .get(machine)
            .map(|m| m.branches_for_hash(hash).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Look up the var_count for the branch with the given branch_id.
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
        reg.insert_branch("main", 0xABCD, 0, 42, 3, vec![]).unwrap();
        reg.insert_branch("main", 0xEF01, 1, 43, 5, vec![None])
            .unwrap();
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
        assert_eq!(reg.var_count_by_branch_id("main", 43), Some(5));
    }

    #[test]
    fn insert_branch_into_unknown_machine_fails() {
        let mut reg = MachineRegistry::default();
        assert!(reg.insert_branch("ghost", 0, 0, 0, 0, vec![]).is_err());
    }

    #[test]
    fn multiple_branches_per_hash() {
        let mut reg = MachineRegistry::default();
        reg.add_machine("fib");
        // 0 i64 and 1 i64 and n i64 all share the same type hash
        let h = 0xDEAD;
        reg.insert_branch("fib", h, 1, 0, 1, vec![Some(GuardValue::Int(0))])
            .unwrap();
        reg.insert_branch("fib", h, 1, 1, 1, vec![Some(GuardValue::Int(1))])
            .unwrap();
        reg.insert_branch("fib", h, 1, 2, 2, vec![None]).unwrap();

        let candidates = reg.branches_for_type_hash("fib", h);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].guards, vec![Some(GuardValue::Int(0))]);
        assert_eq!(candidates[1].guards, vec![Some(GuardValue::Int(1))]);
        assert_eq!(candidates[2].guards, vec![None]);
        assert!(candidates[2].is_wildcard_branch());
    }
}
