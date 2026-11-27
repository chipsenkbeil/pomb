const fs = require('fs');
const src = fs.readFileSync('static/pages.js', 'utf8');
const start = src.indexOf('  const CommitUrl = {');
const end = src.indexOf('\n  };\n', start) + '\n  };\n'.length;
if (start < 0 || end < start) throw new Error('CommitUrl not found');
const CommitUrl = eval('(' + src.slice(start, end).replace(/^\s*const CommitUrl = /, '').replace(/;\s*$/, '') + ')');

const { check, finish } = require('./support/check.js');

check('commit_url_should_name_its_problem accepts an https template with one hash',
  CommitUrl.check('https://git.example.test/commit/{hash}'), '');
check('an http template is accepted', CommitUrl.check('http://git.example.test/c/{hash}'), '');
check('an empty string is accepted', CommitUrl.check(''), '');
check('another scheme is named',
  /https:\/\/ or http:\/\//.test(CommitUrl.check('ftp://git.example.test/{hash}')), true);
check('text with no scheme is named', CommitUrl.check('git.example.test/{hash}') !== '', true);
check('a missing hash is named',
  /exactly once/.test(CommitUrl.check('https://git.example.test/commit')), true);
check('two hashes are named',
  /exactly once/.test(CommitUrl.check('https://git.example.test/{hash}/{hash}')), true);

finish();
