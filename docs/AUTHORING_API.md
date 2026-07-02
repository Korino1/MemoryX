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

## Storage

- Entities are recorded in `meta/entities.jsonl`.
- Relations are recorded in `meta/relations.jsonl`.
- Relation assertions and claims still create real atoms.
- Updates preserve old content through superseding history.
- Deletions create tombstones instead of immediately erasing data.

