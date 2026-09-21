'use strict';

function parseJson(value) {
  if (typeof value !== 'string') return value;
  try { return JSON.parse(value); } catch { return value; }
}

function toolCalls(context) {
  const calls = context.providerResponse?.metadata?.toolCalls;
  return Array.isArray(calls) ? calls : [];
}

function evidenceText(calls) {
  return calls
    .map(call => JSON.stringify(parseJson(call.output ?? '')))
    .join('\n')
    .toLowerCase();
}

function hasNonEmptyResults(value) {
  value = parseJson(value);
  if (!value || typeof value !== 'object') return false;
  if (value.structuredContent !== undefined) return hasNonEmptyResults(value.structuredContent);
  if (Array.isArray(value.results)) return value.results.length > 0;
  return false;
}

module.exports = (output, context) => {
  const text = String(output || '').toLowerCase();
  const config = context.config || {};
  const calls = toolCalls(context);
  const evidence = evidenceText(calls);
  if (config.requireToolEvidence && calls.length === 0) {
    return { pass: false, score: 0, reason: 'No MCP tool result was available for grounding' };
  }
  for (const required of config.evidenceAll || []) {
    if (!evidence.includes(String(required).toLowerCase())) {
      return { pass: false, score: 0, reason: `MCP tool evidence is missing: ${required}` };
    }
  }
  if (config.evidenceAny?.length && !config.evidenceAny.some(value => evidence.includes(String(value).toLowerCase()))) {
    return { pass: false, score: 0, reason: `MCP tool evidence contains none of: ${config.evidenceAny.join(', ')}` };
  }
  if (config.evidenceEmpty && calls.some(call => hasNonEmptyResults(call.output))) {
    return { pass: false, score: 0, reason: 'MCP tool evidence contains an unexpected result' };
  }
  for (const required of config.requiredAll || []) {
    if (!text.includes(String(required).toLowerCase())) {
      return { pass: false, score: 0, reason: `Final answer is missing grounded value: ${required}` };
    }
  }
  if (config.requiredAny?.length && !config.requiredAny.some(value => text.includes(String(value).toLowerCase()))) {
    return { pass: false, score: 0, reason: `Final answer contains none of: ${config.requiredAny.join(', ')}` };
  }
  for (const forbidden of config.forbidden || []) {
    if (text.includes(String(forbidden).toLowerCase())) {
      return { pass: false, score: 0, reason: `Final answer contains unsupported value: ${forbidden}` };
    }
  }
  return { pass: true, score: 1, reason: 'Final answer is consistent with the fixture contract' };
};
