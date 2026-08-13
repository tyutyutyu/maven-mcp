'use strict';

function parseObject(value) {
  if (typeof value !== 'string') return value;
  try { return JSON.parse(value); } catch { return value; }
}

function collectCalls(value, calls, seen = new Set()) {
  value = parseObject(value);
  if (!value || typeof value !== 'object' || seen.has(value)) return;
  seen.add(value);
  const name = value.name || value.toolName || value.tool_name || value.function?.name;
  const input = value.input || value.arguments || value.args || value.function?.arguments;
  if (typeof name === 'string' && input !== undefined) {
    calls.push({ name, input: parseObject(input) });
  }
  for (const nested of Object.values(value)) collectCalls(nested, calls, seen);
}

module.exports = (_output, context) => {
  const expectedTool = context.config?.expectedTool;
  if (!expectedTool) return { pass: true, score: 1, reason: 'No tool expectation for this assertion' };
  const calls = [];
  collectCalls(context.providerResponse, calls);
  collectCalls(context.metadata, calls);
  collectCalls(context.trace, calls);
  const call = calls.find(({ name }) => name === expectedTool || name.endsWith(`__${expectedTool}`));
  if (!call) {
    return { pass: false, score: 0, reason: `Expected ${expectedTool}; observed: ${calls.map(c => c.name).join(', ') || 'no captured tool calls'}` };
  }
  const input = call.input && typeof call.input === 'object' ? call.input : {};
  for (const [key, expected] of Object.entries(context.config.expectedArgs || {})) {
    const actual = input[key];
    if (actual === undefined || !String(actual).toLowerCase().includes(String(expected).toLowerCase())) {
      return { pass: false, score: 0, reason: `${expectedTool}.${key} expected ${expected}, got ${JSON.stringify(actual)}` };
    }
  }
  return { pass: true, score: 1, reason: `${expectedTool} called with grounded arguments` };
};
