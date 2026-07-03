# Security Policy

## Supported Versions

MemoryX is currently pre-1.0.

| Version | Supported |
| --- | --- |
| `main` | Security fixes accepted |
| `0.1.x` releases | Best-effort security fixes |
| Older commits | Not supported |

## Reporting A Vulnerability

Do not open a public GitHub issue for a suspected vulnerability.

Report security issues privately by contacting the repository owner through
GitHub private vulnerability reporting or by opening a private security advisory
if GitHub Security Advisories are enabled for the repository.

If private vulnerability reporting is not available, open a public issue only to
request a private security contact channel. Do not include vulnerability details,
exploit steps, proof of concept, logs, or affected private data in that public
issue.

Include as much detail as possible:

- affected version or commit;
- operating system and build profile;
- whether the `mcp` feature is enabled;
- steps to reproduce;
- expected impact;
- proof of concept, if available;
- whether the issue is already public.

## Expected Process

1. The maintainer acknowledges the report when it is received.
2. The maintainer attempts to reproduce and classify the issue.
3. If confirmed, the maintainer prepares a fix in private when practical.
4. The reporter may be asked to validate the fix.
5. The fix is released or merged.
6. Public disclosure happens after the fix is available, unless another
   timeline is agreed.

## Disclosure Policy

Please do not publicly disclose the vulnerability, exploit details, or proof of
concept until a fix is available and the maintainer has had a reasonable chance
to respond.

If there is no response after 90 days, coordinated disclosure by the reporter is
reasonable.

## Scope

Security-sensitive areas include:

- local database storage and path handling;
- MCP stdio and future network-facing integrations;
- import/export parsers;
- federation endpoints;
- integrity verification, repair, and snapshot logic;
- unsafe code, memory mapping, and platform-specific I/O.

## Non-Security Bugs

For normal bugs, crashes without security impact, documentation issues, or
feature requests, use regular GitHub issues.
