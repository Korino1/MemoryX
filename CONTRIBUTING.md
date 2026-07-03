# Contributing

MemoryX is an open-source project under `AGPL-3.0-or-later` with optional
separate commercial licensing.

External contributions are welcome when the contributor accepts the contributor
license agreement in `CLA.md`. The CLA keeps the project legally able to offer
both:

- the public AGPL version;
- separate commercial licenses for organizations that cannot use AGPL software.

## Contribution Process

1. Fork the repository and create a feature branch.
2. Keep changes focused and include tests or documentation when relevant.
3. Run the local checks before opening a pull request:

```bash
cargo +nightly fmt --check
cargo +nightly check --all-features
cargo +nightly test --all-features
cargo +nightly clippy --all-targets --all-features
```

4. Open a pull request.
5. In the pull request description, keep the CLA confirmation from the template:

```text
I have read and agree to the MemoryX Contributor License Agreement in CLA.md.
```

Maintainers may review and merge external pull requests only when the CLA
confirmation is present and the contribution is compatible with the project
license model.

## Security Issues

Do not report vulnerabilities through public GitHub issues. Follow
`SECURITY.md`.
