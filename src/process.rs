use std::collections::HashSet;

use sysinfo::{Pid, System};

use crate::model::ProcessStats;

pub struct ProcessSampler {
    system: System,
}

impl Default for ProcessSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self {
            system: System::new_all(),
        }
    }

    /// Take a fresh system snapshot. Call once per refresh tick; CPU usage is a
    /// delta between consecutive snapshots, so spacing these by the tick interval
    /// is what makes `sample` report meaningful percentages.
    pub fn refresh(&mut self) {
        self.system.refresh_all();
    }

    pub fn sample(&self, root_pid: u32) -> Option<ProcessStats> {
        let root_pid = Pid::from_u32(root_pid);
        let root = self.system.process(root_pid)?;
        let child_pids = descendant_pids(&self.system, root_pid.as_u32());

        let mut cpu_percent = root.cpu_usage().round().max(0.0) as u32;
        let mut memory_bytes = root.memory();
        for child in &child_pids {
            if let Some(process) = self.system.process(Pid::from_u32(*child)) {
                cpu_percent += process.cpu_usage().round().max(0.0) as u32;
                memory_bytes += process.memory();
            }
        }

        Some(ProcessStats {
            cpu_percent,
            memory_bytes,
            child_pids,
        })
    }
}

fn descendant_pids(system: &System, root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    collect(system, root, &mut seen, &mut found);
    found.sort_unstable();
    found
}

fn collect(system: &System, parent: u32, seen: &mut HashSet<u32>, found: &mut Vec<u32>) {
    for (pid, process) in system.processes() {
        if process.parent().map(|parent_pid| parent_pid.as_u32()) != Some(parent) {
            continue;
        }
        let child = pid.as_u32();
        if seen.insert(child) {
            found.push(child);
            collect(system, child, seen, found);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_reads_snapshot_taken_by_refresh() {
        let mut sampler = ProcessSampler::new();
        sampler.refresh();
        // sample() must read the snapshot refresh() took, not refresh per call,
        // so the dashboard can refresh once per tick and sample every pid from it.
        let stats = sampler
            .sample(std::process::id())
            .expect("the test process itself should be sampled after a refresh");
        assert!(stats.memory_bytes > 0, "memory should be reported in bytes");
    }
}
