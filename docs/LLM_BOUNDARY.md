# LLM Boundary

MemoryX treats external LLMs as proposers and renderers, not as the source of
truth.

## Allowed Operations

- Propose a `QueryContract` from natural language.
- Propose candidate claims for MemoryX validation.
- Propose entity links or relation candidates.
- Render explanations from an already validated `AnswerPack`.

## Forbidden Operations

- Mark a fact as verified.
- Hide or smooth over conflicts.
- Change hard constraints or `MUST_NOT` constraints.
- Invent sources, evidence, line ranges, commits, or provenance.
- Mark an answer complete without MemoryX coverage/proof validation.

## Technical Contract

- Use `Proposal<T>` for LLM output.
- Default proposal status is `proposed`.
- Only MemoryX validation may change status to `accepted_by_memory_x`.
- `AnswerPack.claims` and `AnswerPack.claims_v2` are validated/provenance-bound
  claim surfaces.
- `AnswerPack.proposed_text` is renderer text and must not be interpreted as a
  verified claim.

## Operational Rule

If an LLM proposes a factual statement without claim/evidence/source binding,
MemoryX must treat it as proposed text, hypothesis, or insufficient evidence.
