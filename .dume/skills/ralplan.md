---
name: ralplan
description: Multi-role consensus planning workflow (Planner -> Architect -> Critic) that produces a verified implementation plan.
---

# Ralplan Consensus Planning Workflow

Formulates an implementation plan through role consensus before executing code changes.

## Protocol
1. **Planner Role**:
   - Draft an initial implementation plan with file-by-file breakdown, dependencies, and test strategies.
   - Designate components, risk factors, and rollback mechanisms.
2. **Architect Role Review**:
   - Audit system structure, invariant integrity, backward compatibility constraints, and modularity.
   - Challenge architectural assumptions and tighten component boundaries.
3. **Critic Role Audit**:
   - Scrutinize edge cases, potential concurrency hazards, performance regressions, and failure modes.
   - Require concrete verification commands for every changed component.
4. **Consensus Artifact**:
   - Generate `implementation_plan.md` artifact incorporating consensus feedback.
   - Mark workflow state as `awaiting_approval`. Do NOT start code execution until user approval is confirmed.
