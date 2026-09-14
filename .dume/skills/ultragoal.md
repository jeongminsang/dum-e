---
name: ultragoal
description: Autonomous plan-to-DAG execution workflow with checkpointed worktree verification.
---

# UltraGoal Autonomous Execution Workflow

Converts approved implementation plans into verified Task DAGs and coordinates execution through completion.

## Pipeline
1. **Plan Ingestion**:
   - Ingest approved `implementation_plan.md` artifact.
   - Decompose into granular `Task` nodes with strict file boundaries (`allowed_paths`) and verification commands (`acceptance_criteria`).
2. **DAG Construction & Validation**:
   - Construct `TaskDag`. Validate for cycles, topological dependencies, and completeness.
   - Persist goal and tasks to SQLite store.
3. **Checkpointed Parallel Execution**:
   - Coordinator schedules ready tasks across worktree isolation slots via bounded semaphore.
   - Verify every worktree change using manifest SHA-256 hash before serialization.
4. **Verification & Integration**:
   - Run acceptance test command.
   - Apply atomic compare-and-swap integration commits to target branch.
   - Terminate only when all DAG tasks reach `Completed` status.
