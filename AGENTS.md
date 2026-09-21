# SystemOne contributor instructions

Read README.md and docs/plans/cross-repo.md before implementation. The `s1`
binary, config, HTTP service and OpenJev adapter are implemented; other
adapters are planned. Do not describe planned adapters or releases as shipped.

The cross-repo plan is canonical. Upstream libraries remain independent;
SystemOne owns the common config, service, routing and backend adapters.
Required backend kinds include both Vercel AI Gateway and OpenRouter.

Preserve upstream parity gates. No silent cloud, device or inference fallback.
Keep stdout JSON-only for result commands; logs/errors go to stderr. Never
commit credentials, model weights, private request data or fabricated results.

Pin reviewed upstream revisions. Do not release with sibling path dependencies
or uncommitted upstream changes. Follow milestone verification and progress
requirements in the plan; keep documented status accurate.
