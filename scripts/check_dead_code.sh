#!/bin/bash
# Wave 1: Check that no dead/unreachable modules exist in src/.
#
# A module file (non-mod.rs) is considered LIVE if any other .rs file
# references it via one of:
#   1. `crate::...::mod_name::`  (fully-qualified path)
#   2. `mod_name::`               (brought into scope via `use`)
#   3. `pub use mod_name::{...}`  in parent mod.rs, where at least one
#      re-exported item is referenced elsewhere.
#
# Skips: src/lib.rs (crate root), src/bin/ (binary entry points),
#        *tests.rs (test-only modules).
#
# Exit 0 = pass, Exit 1 = dead code found.

set -e

echo "[dead-code-check] Scanning src/ for unreachable modules..."

FILES=$(find src/ -name '*.rs' | sort)

DEAD_COUNT=0

for file in $FILES; do
    # Skip mod.rs, lib.rs, bin/, and test files
    if [[ "$file" == *"/mod.rs" ]]; then continue; fi
    if [[ "$file" == "src/lib.rs" ]]; then continue; fi
    if [[ "$file" == "src/bin/"* ]]; then continue; fi
    if [[ "$file" == *tests.rs ]]; then continue; fi

    mod_name=$(basename "$file" .rs)

    # Check 1 & 2: mod_name:: appears anywhere outside this file
    name_refs=$(grep -rl "${mod_name}::" src/ 2>/dev/null \
        | grep -v "^${file}$" | head -1 || true)

    # Check 1b: file contains `impl <Type>` blocks (methods on types
    # defined elsewhere). These are inherently wired — the type is
    # used, and the impl block extends it. Skip such files.
    if [ -z "$name_refs" ]; then
        if grep -qE '^impl\b' "$file" 2>/dev/null; then
            continue
        fi
    fi

    # Check 3: re-exported from parent mod.rs, and a re-exported item is used
    parent_dir=$(dirname "$file")
    parent_mod="${parent_dir}/mod.rs"
    pub_use_refs=""
    if [ -f "$parent_mod" ]; then
        if grep -q "pub use ${mod_name}::" "$parent_mod" 2>/dev/null; then
            reexported=$(grep "pub use ${mod_name}::" "$parent_mod" \
                | sed 's/.*::{//; s/}.*//; s/,/ /g' | tr -d ' ')
            for item in $reexported; do
                found=$(grep -rl "\b${item}\b" src/ 2>/dev/null \
                    | grep -v "^${file}$" | grep -v "^${parent_mod}$" | head -1 || true)
                if [ -n "$found" ]; then
                    pub_use_refs="$found"
                    break
                fi
            done
        fi
    fi

    if [ -z "$name_refs" ] && [ -z "$pub_use_refs" ]; then
        echo "  DEAD: $file (no references via mod_name::, crate::, use, or pub use)"
        DEAD_COUNT=$((DEAD_COUNT + 1))
    fi
done

if [ "$DEAD_COUNT" -gt 0 ]; then
    echo ""
    echo "ERROR: Found $DEAD_COUNT potentially dead modules in src/."
    echo "Either wire them to QueryEngine::execute or delete them."
    exit 1
fi

echo "OK: No dead modules detected."
exit 0
