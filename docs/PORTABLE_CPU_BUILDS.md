# Portable and CPU-optimized builds

MemoryX must be distributable across modern CPUs. The default repository build
therefore does not use `target-cpu=native`.

## Build policy

- Public binaries: build portable.
- Local benchmarking: use `target-cpu=native`.
- CPU-family builds: use explicit architecture levels such as `x86-64-v3`,
  `x86-64-v4`, or a named CPU only when the artifact name documents it.
- Runtime hot paths should use CPU feature detection, not global `native`
  compilation.

## Portable release build

Use this for GitHub releases and binaries copied to other machines:

```powershell
cargo +nightly build --release --features mcp
```

This produces a binary that is not tied to the build machine's Zen4 CPU.

## Local native build

Use this only for the current machine:

```powershell
$env:RUSTFLAGS="-C target-cpu=native"
cargo +nightly build --release --features mcp
Remove-Item Env:RUSTFLAGS
```

Do not publish this binary as a generic MemoryX release.

## Explicit x86_64 architecture levels

For modern Intel/AMD machines:

```powershell
$env:RUSTFLAGS="-C target-cpu=x86-64-v3"
cargo +nightly build --release --features mcp
Remove-Item Env:RUSTFLAGS
```

`x86-64-v3` generally implies AVX2/BMI/FMA-era CPUs. It is faster than a fully
generic build, but it will not run on older x86_64 CPUs.

For AVX-512 capable machines:

```powershell
$env:RUSTFLAGS="-C target-cpu=x86-64-v4"
cargo +nightly build --release --features mcp
Remove-Item Env:RUSTFLAGS
```

Use `x86-64-v4` only for a clearly labelled AVX-512 artifact.

## Zen4-specific local build

For a local Zen4-only binary:

```powershell
$env:RUSTFLAGS="-C target-cpu=znver4"
cargo +nightly build --release --features mcp
Remove-Item Env:RUSTFLAGS
```

This is not the default because it can emit instructions unavailable on other
modern CPUs.

## Runtime CPU detection

MemoryX exposes runtime CPU detection in `memoryx::utils::cpu`.

The intended pattern for hot paths is:

```rust
use memoryx::utils::cpu::{runtime_cpu_tier, CpuTier};

match runtime_cpu_tier() {
    CpuTier::X86Avx512 => { /* AVX-512 implementation */ }
    CpuTier::X86Avx2 => { /* AVX2 implementation */ }
    CpuTier::X86Sse41 => { /* SSE4.1 implementation */ }
    CpuTier::Aarch64Neon => { /* NEON implementation */ }
    CpuTier::Portable => { /* scalar implementation */ }
}
```

This lets one portable binary use modern CPU features when they are available,
without crashing on machines that do not support them.

## Current accelerated components

- BLAKE3 uses its own runtime SIMD dispatch on supported targets.
- MemoryX storage uses mmap and platform I/O backends where available.
- New MemoryX-specific SIMD kernels should be added behind runtime CPU checks
  rather than global build flags.
