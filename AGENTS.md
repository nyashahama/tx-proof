# Engineering defaults

- Treat nearest repository instructions as authoritative. Inspect real state and use applicable skills.
- Make small, reviewable changes with acceptance criteria. Use TDD for behavior changes and systematic debugging for failures.
- Preserve user changes; avoid broad cleanup. Inspect visible UI in a browser at relevant viewports.
- Run fresh verification and report exact commands plus residual risks. Prefer primary, license-safe sources.
- Add no dependency, framework, or automation without a concrete need and proof.

## Adaptive delegation

- Default to one coordinator. Spawn only for independent lanes, context isolation, competing hypotheses, or material independent review.
- Use two workers normally; hard maximum three. Do not nest delegation unless the user explicitly asks for hierarchy.
- Give each standalone worker a self-contained work order, not an inherited transcript. It returns its conclusion, evidence, exact paths and commands, uncertainty, and next action.
- One writer works in a shared checkout. Parallel writers need separate worktrees and serial integration.
- Coordinator repair/evaluator loops stop after two passes.
- The optional `loops` pack is off by default. It may exceed two iterations only with an explicit per-run user time, token, or iteration budget. The budget must be finite, positive, and numeric. Selecting the pack alone is not authorization.
- The coordinator retains scope, integration, authorization, user communication, and the completion claim.
