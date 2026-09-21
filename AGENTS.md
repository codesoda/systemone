# SystemOne contributor instructions

Read README.md and docs/plans/cross-repo.md before implementation. This starts
as a planning-only repository: do not describe proposed commands as shipped.

The cross-repo plan is canonical. Upstream libraries remain independent;
SystemOne owns the common config, service, routing and backend adapters.
Required backend kinds include both Vercel AI Gateway and OpenRouter.

Preserve upstream parity gates. No silent cloud, device or inference fallback.
Keep stdout JSON-only for result commands; logs/errors go to stderr. Never
commit credentials, model weights, private request data or fabricated results.

Pin reviewed upstream revisions. Do not release with sibling path dependencies
or uncommitted upstream changes. Follow milestone verification and progress
requirements in the plan; keep documented status accurate.
