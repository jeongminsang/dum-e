---
name: deep-interview
description: Socratic requirements interview workflow that extracts clear goals, acceptance criteria, and constraints before task execution.
---

# Deep Interview Workflow

Run an interactive Socratic requirements interview with the user.

## Instructions
1. Review user's target objective and inspect current codebase context.
2. Ask targeted clarifying questions one at a time across:
   - Primary user goals and acceptance criteria
   - Architecture constraints and forbidden boundaries (e.g. preserved compatibility, dependency pinning)
   - Edge cases, error handling, and test expectations
3. Calculate Ambiguity Score (0 = crystal clear, 100 = completely ambiguous).
4. When Ambiguity Score is below 15, synthesize a structured Requirements Specification:
   - Goal & Problem Statement
   - Scope Boundaries & Invariants
   - Test & Verification Criteria
5. Await explicit user approval before proceeding to plan or execution.
