# Security Policy

## Reporting a vulnerability

Report vulnerabilities **privately** through GitHub security advisories:

**[Report a vulnerability](https://github.com/codesoda/systemone/security/advisories/new)**

Do not open public issues for security reports. We will acknowledge reports,
keep you informed, and credit reporters in the fix's release notes unless you
prefer otherwise.

## Scope

`s1` runs local models on your machine and, when you configure a hosted
backend, talks only to that provider with the credentials you supplied. Reports
of particular interest:

- any path that sends request state, answers, or credentials anywhere other
  than the backend the caller selected;
- `s1 serve` accepting requests without the bearer secret when bound off
  loopback, or leaking the secret in logs, `config show`, or errors;
- the model cache accepting an artifact whose size or SHA-256 does not match
  the pinned manifest, or following a symlink out of the cache;
- the release archive or installer accepting content that does not match
  `SHA256SUMS` and `BUILD-INFO.json`;
- a way to make a backend fall back silently to another device, model, or
  execution mode.

## Supported versions

Security fixes target the latest release. Corrective releases are published
under a new tag; assets under an existing tag are never replaced.
