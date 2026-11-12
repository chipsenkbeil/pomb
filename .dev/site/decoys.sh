set -euo pipefail
dir=/var/lib/pomb/nightly/backups
for name in notes.txt pomb-20200101T000000Z-before-x.redb.partial pomb-20200101T000000Z.redb; do
  if [[ -f "$dir/$name" && ! -s "$dir/$name" ]]; then
    rm -f "$dir/$name"
    echo "removed $name"
  fi
done
