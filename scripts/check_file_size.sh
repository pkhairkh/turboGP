#!/bin/bash
# Wave 11/20: Check that no .rs file in src/ exceeds 2,000 LOC.
# Exit 1 if any file exceeds the limit.
set -e

LIMIT=2000
VIOLATIONS=0

echo "[file-size-check] Scanning src/ for files > ${LIMIT} LOC..."

while IFS= read -r file; do
    loc=$(wc -l < "$file")
    if [ "$loc" -gt "$LIMIT" ]; then
        echo "  VIOLATION: $file ($loc lines, limit $LIMIT)"
        VIOLATIONS=$((VIOLATIONS + 1))
    fi
done < <(find src/ -name '*.rs' -type f)

if [ "$VIOLATIONS" -gt 0 ]; then
    echo ""
    echo "ERROR: $VIOLATIONS file(s) exceed the ${LIMIT}-LOC limit."
    echo "Decompose large files into smaller modules."
    exit 1
fi

echo "OK: All files within ${LIMIT}-LOC limit."
exit 0
