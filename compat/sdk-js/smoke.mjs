import assert from "node:assert/strict";
import { TypeSafeClient, choice, noul, score } from "@typesafe-ai/sdk";

const baseURL = process.env.SYSTEMONE_BASE_URL ?? "http://127.0.0.1:8080";
const apiKey = process.env.SYSTEMONE_API_KEY ?? "systemone-local-sdk-smoke";
// Which backend kind the server is running; only backend-specific limits
// differ. openjev: float state values are rejected (422). laya and gliner2:
// accepted. typesafe is the hosted TypeSafe API behind the adapter: it keeps
// the upstream catalogue, model identity, label order and token counters, so
// four shared checks relax for it and for it alone.
const backendKind = process.env.SYSTEMONE_SMOKE_BACKEND ?? "openjev";
if (!["openjev", "laya", "gliner2", "typesafe"].includes(backendKind)) {
  throw new Error(
    `SYSTEMONE_SMOKE_BACKEND must be openjev, laya, gliner2 or typesafe, got ${backendKind}`,
  );
}
const hosted = backendKind === "typesafe";
// The float-state outcome for the TypeSafe adapter. The adapter forwards state
// values unchanged and the hosted API accepts floats, so the request answers.
// docs/plans/cross-repo.md records that decision; move this constant only with
// the adapter.
const typesafeFloatState = "accepted";
const requestedModel = "jev-latest";
const client = new TypeSafeClient({
  apiKey,
  baseURL,
  timeout: 120_000,
  retry: { maxRetries: 0 },
});

const models = await client.models.list();
// A local backend holds exactly one resident model. TypeSafe forwards its own
// catalogue as-is, which lists more than one model.
if (hosted) {
  if (models.length < 1) throw new Error("typesafe reported an empty model catalogue");
} else if (models.length !== 1) {
  throw new Error(`expected one resident model, got ${models.length}`);
}

const result = await client.systemOne({
  model: requestedModel,
  state: { ticket: "A duplicate charge needs review", severity: 3 },
  questions: {
    route: choice("Which queue should own this?", {
      billing: "Payment and duplicate-charge problems",
      support: "General product support",
    }),
    review: noul("Does this need human review?", {
      true: "A human should inspect it",
      false: "Automation can safely close it",
    }),
    urgency: score("How urgent is this?", ["low", "medium", "high"]),
  },
});

for (const card of models) {
  assert.equal(typeof card.name, "string");
  assert.ok(card.name.length > 0);
  assert.equal(typeof card.description, "string");
  assert.equal(typeof card.release_date, "string");
}
if (hosted) {
  // TypeSafe resolves the alias to a concrete upstream identity (for example
  // jev-latest to jev-1.13.0) and SystemOne passes model identity through
  // verbatim, so the answer names a model of the requested family that the
  // catalogue does not have to list.
  assert.equal(typeof result.model, "string");
  assert.ok(result.model.length > 0 && result.model.trim() === result.model);
  assert.equal(
    result.model.split("-")[0],
    requestedModel.split("-")[0],
    `hosted model ${result.model} must stay in the ${requestedModel.split("-")[0]} family`,
  );
} else {
  assert.equal(result.model, models[0].name);
}
assert.deepEqual(Object.keys(result.answers), ["route", "review", "urgency"]);
const probability = (value) => {
  assert.equal(typeof value, "number");
  assert.ok(Number.isFinite(value) && value >= 0 && value <= 1);
  assert.ok(Math.abs(value * 100 - Math.round(value * 100)) < 1e-9);
};
// `upstreamOrder` holds only where a hosted provider owns the key order.
const distribution = (values, labels, upstreamOrder = false) => {
  if (upstreamOrder) {
    assert.deepEqual(Object.keys(values).sort(), [...labels].sort());
  } else {
    assert.deepEqual(Object.keys(values), labels);
  }
  Object.values(values).forEach(probability);
  // Independent two-decimal rounding need not sum to precisely one.
  assert.ok(Math.abs(Object.values(values).reduce((a, b) => a + b, 0) - 1) <= labels.length * 0.005 + 1e-9);
};
const { route, review, urgency } = result.answers;
assert.equal(route.type, "choice");
assert.ok(["billing", "support"].includes(route.choice));
probability(route.confidence);
// TypeSafe returns the choice labels in its own order and SystemOne preserves
// it; local backends must keep the declared order.
distribution(route.probabilities, ["billing", "support"], hosted);
assert.equal(review.type, "noul");
probability(review.noul);
assert.equal(urgency.type, "score");
assert.ok(Number.isFinite(urgency.score) && urgency.score >= 0 && urgency.score <= 2);
assert.ok(Math.abs(urgency.score * 100 - Math.round(urgency.score * 100)) < 1e-9);
assert.deepEqual(urgency.legend, { "0": "low", "1": "medium", "2": "high" });
probability(urgency.confidence);
// Score probabilities carry index keys that SystemOne re-emits by position for
// every backend, so the order stays exact.
distribution(urgency.probabilities, ["0", "1", "2"]);
assert.ok(Number.isSafeInteger(result.usage.input_tokens) && result.usage.input_tokens > 0);
if (hosted) {
  // SystemOne forwards the counter the hosted API reported, so this leg pins
  // its shape and not its value; a local backend emits none.
  assert.ok(Number.isSafeInteger(result.usage.output_tokens) && result.usage.output_tokens >= 0);
} else {
  assert.equal(result.usage.output_tokens, 0);
}

// Every backend, hosted included, resolves the model name against its own
// configuration before it dispatches, so an unknown model is a 404 here even
// though the hosted API answers 400 for one.
let rejected = false;
try {
  await client.systemOne({
    model: "not-the-loaded-model",
    state: "x",
    questions: { q: noul("?") },
  });
} catch (error) {
  rejected = error?.status === 404;
}
if (!rejected) throw new Error("unknown model did not produce the SDK's 404 error path");

let floatOutcome = "accepted";
try {
  const withFloat = await client.systemOne({
    state: { floatValue: 1.5 },
    questions: { q: noul("?") },
  });
  probability(withFloat.answers.q.noul);
} catch (error) {
  floatOutcome = error?.status === 422 ? "rejected-422" : `unexpected:${error?.status ?? error}`;
}
const expectedFloatOutcome = hosted
  ? typesafeFloatState
  : backendKind === "openjev"
    ? "rejected-422"
    : "accepted";
if (expectedFloatOutcome === "rejected-422") {
  assert.equal(
    floatOutcome,
    "rejected-422",
    `${backendKind} must reject float state values with the SDK's 422 path`,
  );
} else {
  assert.equal(floatOutcome, "accepted", `${backendKind} must accept float state values`);
}

console.log(JSON.stringify({
  sdk: "@typesafe-ai/sdk@0.6.0",
  source_commit: "66880ccded6cb642dc1809620c2b108c33730214",
  baseURL,
  backend: backendKind,
  model: result.model,
  answers: Object.keys(result.answers),
  usage: result.usage,
  status: "passed",
}));
