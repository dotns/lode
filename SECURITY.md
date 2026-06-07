# Security Policy

## Supported Versions

lode is pre-1.0 software and follows `0.0.x` versioning. Only the **latest
released version** receives security updates. Older releases are not patched —
please upgrade to the most recent release before reporting an issue.

| Version | Supported          |
| ------- | ------------------ |
| latest `0.0.x` | :white_check_mark: |
| any older release | :x:           |

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**
Public disclosure before a fix is available puts all users at risk.

Report vulnerabilities privately through GitHub Security Advisories:

1. Go to <https://github.com/dotns/lode/security/advisories>.
2. Click **"Report a vulnerability"**.
3. Fill in the advisory form with the details below.

This opens a private channel visible only to the maintainers and you.

### What to include

To help us triage and reproduce quickly, please provide:

- A clear description of the vulnerability and its impact.
- The affected version (and platform/OS, if relevant).
- Step-by-step reproduction instructions or a proof of concept.
- Any relevant logs, manifests, or configuration (with secrets redacted).
- Your assessment of severity and possible mitigations, if known.

## Response Timeline

- **Acknowledgement:** we aim to acknowledge your report within **72 hours**.
- **Triage:** we will assess and confirm the issue, and share an initial
  severity assessment, within **7 days** of acknowledgement.
- **Fix:** for confirmed vulnerabilities we target a fix or mitigation in the
  next release, prioritized by severity. We will keep you updated on progress.

## Coordinated Disclosure

We follow coordinated disclosure. Please give us a reasonable window to
investigate and ship a fix before any public disclosure. Once a fix is
released, we will publish a security advisory and credit the reporter (unless
you prefer to remain anonymous). We ask that you do not disclose the issue
publicly until the advisory is published.

## Scope

This policy covers the lode loader/supervisor and its update and verification
path, including:

- The loader/supervisor binary and its process-management behavior.
- The update mechanism (manifest handling, download, and version selection).
- The verification path: integrity checking (hashes) and publisher-identity
  verification (signatures).

Issues in third-party dependencies should be reported upstream, but you are
welcome to notify us if lode's use of a dependency is exploitable.
