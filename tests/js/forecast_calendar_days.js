process.env.TZ = 'America/New_York';
const fs = require('fs');
const src = fs.readFileSync(process.argv[2], 'utf8');
const { LocalTime, ReportChart, ReportFile, escapeHtml, isoDate } = require('../../static/pure.js');
LocalTime.use({ hour12: true });
const block = (start) => {
  const from = src.indexOf(start);
  if (from < 0) throw new Error(start + ' not found');
  const to = src.indexOf('\n  })();', from) + '\n  })();'.length;
  return src.slice(from, to);
};
const document = { getElementById: () => null };
eval(block('  var ReportBreakdown = (function () {'));
eval(block('  var MonteCarloPage = (function () {'));

const { check, finish } = require('./support/check.js');

Math.random = () => 0;
const ymd = (ms) => {
  const d = new Date(ms);
  return [d.getFullYear(), d.getMonth() + 1, d.getDate(), d.getHours()];
};

const autumn = MonteCarloPage.runSimulation([1], 3, 2, new Date(2026, 9, 30).getTime());
check('forecast-dates-step-by-calendar-day-autumn', autumn.map(ymd), [[2026, 11, 2, 0], [2026, 11, 2, 0]]);

const spring = MonteCarloPage.runSimulation([1], 3, 1, new Date(2026, 2, 6).getTime());
check('forecast-dates-step-by-calendar-day-spring', spring.map(ymd), [[2026, 3, 9, 0]]);

const springDays = [6, 7, 8, 9, 10].map((d) => new Date(2026, 2, d).getTime());
const binned = MonteCarloPage.bucketForecast(springDays);
check('forecast-dates-step-by-calendar-day-spring histogram gives each local day one bucket', binned.buckets, [1, 1, 1, 1, 1]);
check('forecast-dates-step-by-calendar-day-spring histogram starts at local midnight', ymd(binned.first), [2026, 3, 6, 0]);

const away = MonteCarloPage.runSimulation([1], 3, 1, new Date(2026, 5, 10).getTime());
check('forecast-dates-away-from-a-clock-change', away.map(ymd), [[2026, 6, 13, 0]]);
const awayBins = MonteCarloPage.bucketForecast([new Date(2026, 5, 10, 15).getTime(), new Date(2026, 5, 12).getTime()]);
check('forecast-dates-away-from-a-clock-change histogram', awayBins.buckets, [1, 0, 1]);

finish();
