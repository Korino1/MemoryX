# Authoring API

MemoryX authoring writes durable atoms, entities, relations, and history. It is
not direct text-chunk insertion.

## CLI

Create an entity:

```bash
memoryx create-entity --name GPU --entity-type hardware
```

Add an atom-backed claim:

```bash
memoryx add-entity-claim --entity 1 --predicate 7 --object 4090 --object-tag u64
```

Create an atom-backed relation:

```bash
memoryx create-relation --subject 1 --predicate 8 --object 2 --ctx 0
```

## MCP

Create entity:

```json
{"name":"create_entity","arguments":{"canonical_name":"GPU","entity_type":"hardware","aliases":["graphics-card"]}}
```

Add claim:

```json
{"name":"add_claim","arguments":{"entity_id":1,"predicate":7,"object":4090,"object_tag":"U64","ctx_id":0}}
```

Assert relation:

```json
{"name":"assert_relation","arguments":{"subject":1,"predicate":8,"object":2,"ctx_id":0}}
```

Correct relation:

```json
{"name":"correct_relation","arguments":{"relation_id":1,"subject":1,"predicate":8,"object":3,"ctx_id":0}}
```

Transition one current relation value:

```json
{"name":"transition_relation","arguments":{"old_relation_id":1,"new_object":3,"ctx_id":0,"source_ids":[1,2]}}
```

Use `transition_relation` for state predicates such as a `ManyToOne` session
state. It keeps the subject and predicate, validates the new object and
predicate contract, replaces the old active claim in the selected context,
adds a superseding relation/atom, attaches every listed registered source, and
records the transition in durable history. Repeating the operation against the
already superseded relation fails closed.

`correct_relation` remains available for general corrections that may also
change subject or predicate. It does not express the same single-current-value
transition contract.

## Storage

- Entities are recorded in `meta/entities.jsonl`.
- Relations are recorded in `meta/relations.jsonl`.
- Relation assertions and claims still create real atoms.
- Updates preserve old content through superseding history.
- Deletions create tombstones instead of immediately erasing data.
- Relation transitions are prevalidated and serialized by the owning process.
  Their logical state/history contract is durable across normal close and
  reopen. MemoryX does not currently claim power-loss atomicity across every
  underlying CAS, context, relation, source-link, and history file.
