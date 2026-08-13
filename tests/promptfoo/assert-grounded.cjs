'use strict';

module.exports = (output, context) => {
  const text = String(output || '').toLowerCase();
  const config = context.config || {};
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
