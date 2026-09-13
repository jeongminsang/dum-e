use crate::types::{Task, TaskStatus};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DagError {
    #[error("Cycle detected in task dependencies: {0:?}")]
    CycleDetected(Vec<String>),
    #[error("Missing dependency: task {task_id} depends on nonexistent task {dep_id}")]
    MissingDependency { task_id: String, dep_id: String },
}

#[derive(Debug)]
pub struct TaskDag<'a> {
    tasks: HashMap<&'a str, &'a Task>,
}

impl<'a> TaskDag<'a> {
    pub fn new(tasks: &'a [Task]) -> Result<Self, DagError> {
        let mut map = HashMap::new();
        for task in tasks {
            map.insert(task.id.as_str(), task);
        }

        // Validate dependencies exist
        for task in tasks {
            for dep in &task.dependencies {
                if !map.contains_key(dep.as_str()) {
                    return Err(DagError::MissingDependency {
                        task_id: task.id.clone(),
                        dep_id: dep.clone(),
                    });
                }
            }
        }

        let dag = Self { tasks: map };
        dag.validate_acyclic()?;
        Ok(dag)
    }

    fn validate_acyclic(&self) -> Result<(), DagError> {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for &id in self.tasks.keys() {
            if !visited.contains(id) {
                self.dfs_check_cycle(id, &mut visited, &mut rec_stack)?;
            }
        }
        Ok(())
    }

    fn dfs_check_cycle(
        &self,
        node: &'a str,
        visited: &mut HashSet<&'a str>,
        rec_stack: &mut HashSet<&'a str>,
    ) -> Result<(), DagError> {
        visited.insert(node);
        rec_stack.insert(node);

        if let Some(task) = self.tasks.get(node) {
            for dep in &task.dependencies {
                let dep_str = dep.as_str();
                if !visited.contains(dep_str) {
                    self.dfs_check_cycle(dep_str, visited, rec_stack)?;
                } else if rec_stack.contains(dep_str) {
                    return Err(DagError::CycleDetected(vec![node.to_string(), dep.clone()]));
                }
            }
        }

        rec_stack.remove(node);
        Ok(())
    }

    pub fn get_ready_tasks(&self) -> Vec<&'a Task> {
        let completed_ids: HashSet<&'a str> = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| t.id.as_str())
            .collect();

        self.tasks
            .values()
            .copied()
            .filter(|t| {
                (t.status == TaskStatus::Ready || t.status == TaskStatus::Blocked)
                    && t.dependencies.iter().all(|d| completed_ids.contains(d.as_str()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_task(id: &str, status: TaskStatus, deps: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            goal_id: "goal_1".to_string(),
            title: format!("Task {}", id),
            description: "".to_string(),
            status,
            dependencies: deps.into_iter().map(String::from).collect(),
            acceptance_criteria: vec![],
            allowed_paths: None,
            target_branch: "main".to_string(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_ready_tasks_resolution() {
        let tasks = vec![
            make_test_task("t1", TaskStatus::Completed, vec![]),
            make_test_task("t2", TaskStatus::Blocked, vec!["t1"]),
            make_test_task("t3", TaskStatus::Blocked, vec!["t2"]),
        ];

        let dag = TaskDag::new(&tasks).unwrap();
        let ready = dag.get_ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "t2");
    }

    #[test]
    fn test_cycle_detection() {
        let tasks = vec![
            make_test_task("t1", TaskStatus::Blocked, vec!["t2"]),
            make_test_task("t2", TaskStatus::Blocked, vec!["t1"]),
        ];

        let err = TaskDag::new(&tasks).unwrap_err();
        match err {
            DagError::CycleDetected(_) => {}
            _ => panic!("Expected cycle error"),
        }
    }
}
