(() => {
  const ApiDocs = {
    terms: (query) => query.toLowerCase().split(/\s+/).filter(Boolean),
    matches: (text, terms) => terms.every((term) => text.includes(term)),
    summary: (shown, total, terms) => {
      if (!terms.length) return '';
      return `${shown} of ${total} ${total === 1 ? 'entry' : 'entries'}`;
    },
  };

  const input = document.getElementById('api-search');
  if (!input) return;
  const count = document.getElementById('api-count');
  const entries = [...document.querySelectorAll('[data-api-entry]')].map((section) => {
    const link = document.querySelector(`a[href="#${section.id}"]`);
    return { section, item: link ? link.parentNode : null, text: section.textContent.toLowerCase() };
  });
  const intros = [...document.querySelectorAll('[data-api-intro]')];
  const groups = [...document.querySelectorAll('[data-api-group]')];

  const filter = () => {
    const terms = ApiDocs.terms(input.value);
    let shown = 0;
    for (const entry of entries) {
      const hit = ApiDocs.matches(entry.text, terms);
      entry.section.hidden = !hit;
      if (entry.item) entry.item.hidden = !hit;
      if (hit) shown++;
    }
    for (const intro of intros) intro.hidden = terms.length > 0;
    for (const group of groups) {
      group.hidden = !group.querySelector('[data-api-entry]:not([hidden]), [data-api-link]:not([hidden])');
    }
    count.textContent = ApiDocs.summary(shown, entries.length, terms);
  };

  input.addEventListener('input', filter);
})();
