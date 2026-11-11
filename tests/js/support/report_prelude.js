const extractors = (src) => {
  const block = (start) => {
    const from = src.indexOf(start);
    if (from < 0) throw new Error(start + ' not found');
    const to = src.indexOf('\n  })();', from) + '\n  })();'.length;
    return src.slice(from, to);
  };
  const fn = (start) => {
    const from = src.indexOf(start);
    if (from < 0) throw new Error(start + ' not found');
    return src.slice(from, src.indexOf('\n  }\n', from) + '\n  }\n'.length);
  };
  return { block, fn };
};

const accountPrefs = (weekStart) => ({ hour12: true, weekStart });

module.exports = { extractors, accountPrefs };
