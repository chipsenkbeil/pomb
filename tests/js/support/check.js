let failures = 0;

const check = (name, got, want) => {
  const g = JSON.stringify(got);
  const w = JSON.stringify(want);
  if (g !== w) {
    failures++;
    console.log('FAIL ' + name + ': got ' + g + ' want ' + w);
  }
};

const finish = () => {
  if (failures) process.exit(1);
  console.log('ok');
};

module.exports = { check, finish };
