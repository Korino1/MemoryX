# MemoryX Orchestration Operations

`manifest.json` is the module registry. `session_registry.json` records the
observed session binding state and must match every `session_id.txt` exactly.
`ARCHITECTURE.md` defines authority, ownership, sessions, recovery, storage,
EvidenceReturn, and validation limits.

## Build Or Check The Contours

Materialize only missing files. Existing task, plan, progress, decision, and
acceptance files are never overwritten:

```powershell
pwsh -NoLogo -File ORCHESTRATION_SYSTEM/scripts/build_scheme.ps1
```

Check that every declared contour is complete without writing files:

```powershell
pwsh -NoLogo -File ORCHESTRATION_SYSTEM/scripts/build_scheme.ps1 -Check
```

## Initialize Project-Local MemoryX Bases

```powershell
pwsh -NoLogo -File ORCHESTRATION_SYSTEM/scripts/initialize_memoryx.ps1 `
  -Module all `
  -MemoryXBinary .\target\release\memoryx.exe
```

The only allowed base root is `<repo>/.memoryx/bases`. The root uses
`.memoryx/bases/memoryx`; each module uses the unique base declared in its
`module.json` and `MEMORYX_CONTRACT.json`. Existing nonempty bases are not
force-initialized.

## Validate

Validate one contour:

```powershell
pwsh -NoLogo -File ORCHESTRATION_SYSTEM/scripts/validate_module.ps1 `
  -Module MX-40 `
  -RequireBase
```

Validate the full registry and every physical base declaration:

```powershell
pwsh -NoLogo -File ORCHESTRATION_SYSTEM/scripts/run_all_validations.ps1 `
  -RequireBases
```

Add `-IncludeCargoGates` only after shared-host coordination permits heavy
work. The resource coordinator waits; it never stops foreign processes.

## Activate Or Resume A Module

First write a bounded real task and plan into the selected contour. Inspect the
command without creating a session:

```powershell
pwsh -NoLogo -File ORCHESTRATION_SYSTEM/scripts/invoke_module.ps1 `
  -Module MX-40 `
  -DryRun
```

Start or resume the permanent module session:

```powershell
pwsh -NoLogo -File ORCHESTRATION_SYSTEM/scripts/invoke_module.ps1 `
  -Module MX-40
```

An empty `session_id.txt` means `UNBOUND`. On first start the script writes a
UUID only if it observes a canonical UUID in Codex `thread.started` JSON. A
bound module always uses `codex exec resume <UUID>` and reasserts
`gpt-5.6-sol` plus `xhigh`. It never uses a hook-trust bypass and never invents
a UUID. `max` is forbidden.

`session_registry.json` is not an alternate binding authority: the contour's
`session_id.txt` remains authoritative, while aggregate validation fails if the
registry differs. Currently only MX-95 is bound to a real observed UUID.

## Compact Recovery

Before compact, update `TASK.md`, `PLAN.md`, `PROGRESS.md`, and `DECISIONS.md`,
then record their state:

```powershell
pwsh -NoLogo -File <module>/hooks/PreCompact.ps1 `
  -AcceptanceState ready_for_compact
```

After a real compact, load only the saved recovery packet:

```powershell
pwsh -NoLogo -File <module>/hooks/PostCompact.ps1
```

`SessionStart.ps1` validates the contour and reports its saved session/recovery
state. Direct script execution proves only script behavior; it does not prove
that Codex invoked a hook, that compact occurred, that cache was reused, or
that model output met quality gates.

## Evidence Return

Every contour contains `evidence/EVIDENCE_RETURN.example.json` and references
`schemas/evidence-return.schema.json`. Replace the initial `not_run` values
only with observed commands, artifacts, MemoryX provenance, conflicts,
unknowns, and next steps. The report cannot serve as its own evidence.
