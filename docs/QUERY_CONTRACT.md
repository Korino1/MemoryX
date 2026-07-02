# QueryContract

`QueryContract` is the explicit query boundary used by CLI, MCP, and the
native API. Natural language is compiled into this structure before execution,
so agents can inspect and edit the request instead of relying on hidden prompt
state.

## CLI

Compile a natural query without executing it:

```bash
memoryx --format json query --emit-contract "Explain MemoryX MCP and require provenance"
```

Execute a saved contract:

```bash
memoryx --format json query --contract contract.json
```

Structured query output returns an AnswerPack-shaped JSON/YAML payload.

## MCP

Compile:

```json
{"name":"compile_query_contract","arguments":{"query_text":"Explain MemoryX MCP"}}
```

Validate:

```json
{"name":"validate_query_contract","arguments":{"contract":{"intent":"lookup","targets":[{"label":"term:1"}],"relations":[],"constraints":[],"quantifiers":[]}}}
```

Execute:

```json
{"name":"query","arguments":{"query_text":"What decisions mention persistence?","ctx_id":0}}
```

Or execute a strict contract:

```json
{"name":"query","arguments":{"contract":{"intent":"lookup","targets":[{"label":"term:1"}],"relations":[],"constraints":[]},"ctx_id":0}}
```

## Main Fields

- `intent`: lookup, define, explain, compare, derive, verify, or plan.
- `targets`: explicit entities, labels, aliases, or domain masks.
- `relations`: required relation patterns.
- `constraints`: MUST, MUST_NOT, and SHOULD rules.
- `temporal_scope`: before/after/range/valid-at/latest constraints.
- `context_scope`: active, branch, project, user-global, or named selectors.
- `conflict_policy`: fail, branch, include alternatives, prefer trusted, or prefer recent.
- `output_contract`: answer graph/provenance/trace preferences.
- `budgets`: iteration, atom, edge, I/O, time, and federation limits.

