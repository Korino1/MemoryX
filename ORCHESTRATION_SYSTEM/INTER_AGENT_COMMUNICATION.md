# Inter-Agent Communication Contract

Authority: `MX-ROOT`

## Required Language

Inter-agent language: English only.

Every prompt sent to an autonomous contour, task packet, plan, progress or
decision narrative, cross-module handoff, EvidenceReturn narrative, compact
recovery instruction, and wrapper stable prefix must be written in English.
An orchestrator must translate a user request into an English task packet
before invoking another agent. User-facing responses may follow the user's
language, but user-facing text must not be copied untranslated into an
inter-agent artifact.

The model and reasoning profile remain `gpt-5.6-sol` and `xhigh`. This language
contract does not change session binding, ownership, local-base, evidence,
MemoryX concept, or roadmap rules.

## Machine-Enforced Boundary

The validators fail closed when:

- a required contour lacks the exact language marker;
- an inter-agent communication surface contains a non-ASCII Unicode letter or
  combining mark outside the explicit technical-literal allowlist;
- an EvidenceReturn or compact recovery record does not declare English;
- the invocation wrapper does not validate both the supplied task prompt and
  its stable prefix;
- generated contours do not inherit this contract.

The current technical-literal allowlist contains only
`Concept/Расширение.txt`, because that is an immutable repository path rather
than narrative prose. Adding another exception requires an explicit root
decision and a validator update.

The lexical gate is deterministic and dependency-free. It cannot prove that
arbitrary ASCII prose is semantically English, so human/module acceptance must
still reject non-English Latin-script prose. Structural validation must not
claim natural-language understanding.

## Persistence And Recovery

The rule is stored in versioned files and is not delegated to MemoryX or model
memory. `SessionStart`, `PreCompact`, and `PostCompact` expose the declared
language in their machine-readable output. Hooks restore only saved English
instructions; they do not translate or create semantic content.

## Change Control

This contract is additive. It must not weaken atoms, contexts, conflicts,
fixed-point proof assembly, AnswerGraph, provenance, federation, CAS/CRDT,
scoped storage, one-owner leases, immutable sessions, or the open N5 roadmap
gate.
