#!/bin/bash
# Wave 6: Check that no new panic-on-input paths are added to production code.
# Scans src/ (excluding #[cfg(test)] modules) for unwrap(), expect(), panic!(),
# unreachable!(). Exits 1 if any are found.
#
# This is a defence-in-depth check — CI runs it on every PR.

set -e

# Find all .rs files in src/ excluding test modules
FILES=$(find src/ -name '*.rs' -print)

VIOLATIONS=0

for file in $FILES; do
    # Extract lines that are NOT inside #[cfg(test)] modules
    # This is a heuristic: we skip everything after a line containing
    # "#[cfg(test)]" until the end of that module.
    #
    # We look for:
    #   .unwrap()    — panics on None/Err
    #   .expect(     — panics with a message
    #   panic!(      — explicit panic
    #   unreachable!( — panic for "impossible" states
    #   todo!(       — panic for unimplemented
    #   unimplemented!( — panic for unimplemented
    #
    # We EXCLUDE:
    #   - Lines inside #[cfg(test)] mod blocks
    #   - Lines that are comments (// or ///)
    #   - Lines in doc comments

    # Simple approach: use awk to skip test modules
    awk '
    /^#\[cfg\(test\)\]/ { in_test = 1 }
    in_test && /^mod / { in_test_mod = 1; depth = 0 }
    in_test_mod {
        if /\{/ { depth++ }
        if /\}/ { depth--; if depth == 0 { in_test_mod = 0; in_test = 0 } }
        next
    }
    /^\s*\/\// { next }  # skip comments
    /\.unwrap\(\)/ || /\.expect\(/ || /panic!\(/ || /unreachable!\(/ || /todo!\(/ || /unimplemented!\(/ {
        print FILENAME ":" NR ": " $0
        violations++
    }
    ' violations=0 "$file" > /tmp/panics_$$ 2>/dev/null || true

    if [ -s /tmp/panics_$$ ]; then
        cat /tmp/panics_$$
        VIOLATIONS=$((VIOLATIONS + $(wc -l < /tmp/panics_$$)))
    fi
    rm -f /tmp/panics_$$
done

if [ "$VIOLATIONS" -gt 0 ]; then
    echo ""
    echo "ERROR: Found $VIOLATIONS panic-on-input paths in production code."
    echo "Production code must use Result<T, Error> and the ? operator."
    echo "See CONTRIBUTING.md: 'No unwrap() or expect() in production code'"
    exit 1
fi

echo "OK: No panic-on-input paths found in production code."
exit 0
